use super::*;
use async_trait::async_trait;
use domain::ports::{ModelProvider, ProviderFacets};

pub fn parse_chatgpt_wham_usage(body: &Value, observed_at: DateTime<Utc>) -> (Option<String>, Vec<QuotaWindow>, Option<ResetGrant>) {
    let limits = &body["rate_limit"];
    let mut readings = ["primary_window", "secondary_window"].into_iter().filter_map(|key| limits[key]["used_percent"].as_f64());
    // A fractional reading only establishes the scale if no window reports percentage points.
    let fractional = !readings.clone().any(|p| p > 1.0) && readings.any(|p| p > 0.0 && p < 1.0);
    let mut windows = Vec::new();
    for (key, label, kind) in
        [("primary_window", "5-hour", QuotaWindowKind::Session), ("secondary_window", "Weekly", QuotaWindowKind::Weekly)]
    {
        let Some(raw) = limits.get(key).filter(|w| w.is_object()) else { continue };
        let used = raw["used_percent"].as_f64().map(|p| (if fractional { p * 100.0 } else { p }).clamp(0.0, 100.0));
        let mut window = percent_window(key, label, used, observed_at);
        window.kind = kind;
        window.resets_at = seconds_or_iso(&raw["reset_at"]);
        window.window_duration_mins = uint(&raw["limit_window_seconds"]).filter(|seconds| *seconds >= 60).map(|seconds| seconds / 60);
        if let Some(minutes) = window.window_duration_mins {
            (window.label, window.kind) = match minutes {
                300 => ("5-hour".into(), QuotaWindowKind::Session),
                1440 => ("Daily".into(), QuotaWindowKind::Daily),
                10080 => ("Weekly".into(), QuotaWindowKind::Weekly),
                _ => (format!("{minutes}-minute window"), QuotaWindowKind::Custom),
            };
        }
        windows.push(window);
    }
    if limits["allowed"] == false || limits["limit_reached"] == true {
        let active = |w: &QuotaWindow| w.resets_at.is_none_or(|t| t > observed_at);
        let full = windows.iter().filter(|w| active(w)).any(|w| w.used_percent == Some(100.0));
        let reject = |w: &mut QuotaWindow| {
            w.status = QuotaStatus::Rejected;
            w.observation_state = QuotaObservationState::Measured;
        };
        for w in &mut windows {
            if active(w) && w.used_percent == if full { Some(100.0) } else { None } {
                reject(w);
            }
        }
        if !windows.iter().any(|w| w.status == QuotaStatus::Rejected) {
            // Prefer an actual future reset; expired timestamps cannot lift an explicit denial.
            if let Some(w) = windows.iter_mut().min_by_key(|w| (!active(w), w.resets_at.unwrap_or(DateTime::<Utc>::MAX_UTC))) {
                w.resets_at = w.resets_at.filter(|t| *t > observed_at);
                reject(w);
            } else {
                let mut window = percent_window("rate_limit_denied", "Rate limit", None, observed_at);
                window.kind = QuotaWindowKind::Session;
                reject(&mut window);
                windows.push(window);
            }
        }
    }
    let credits = &body["credits"];
    if credits.is_object() && credits["unlimited"].as_bool() != Some(true) {
        let balance = credits["balance"].as_f64().or_else(|| str_field(&credits["balance"])?.parse::<f64>().ok()).filter(|n| n.is_finite());
        // QuotaWindow has no remaining_amount; balance is neither the spent amount nor a limit.
        let label = balance.map_or("Credits".into(), |n| format!("Credits · {:.2} remaining", n / 100.0));
        let mut window = percent_window("credits", &label, None, observed_at);
        window.unit = QuotaUnit::Credits;
        if balance.is_some() {
            window.observation_state = QuotaObservationState::Measured;
        }
        windows.push(window);
    }
    let grants = ["reset_credits", "rate_limit_reset_credits", "reset_grant"]
        .into_iter()
        .find_map(|key| body.get(key).filter(|v| v.is_object()))
        .unwrap_or(&Value::Null);
    let grant = reset_grants(grants, observed_at).into_iter().max_by_key(|g| (g.applicable, g.available_count > 0));
    (str_field(&body["plan_type"]).map(str::to_owned), windows, grant)
}

/// Use a no-redirect HTTP client for authenticated provider probes.
pub async fn probe_chatgpt_wham_usage(
    http: &Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<(Option<String>, Vec<QuotaWindow>, Option<ResetGrant>), String> {
    let mut request = http.get("https://chatgpt.com/backend-api/wham/usage").bearer_auth(access_token);
    if let Some(id) = account_id {
        request = request.header("ChatGPT-Account-Id", id);
    }
    let body = probe_json(request, "ChatGPT WHAM usage").await?;
    let parsed = parse_chatgpt_wham_usage(&body, Utc::now());
    if parsed.1.is_empty() && parsed.2.is_none() {
        Err("ChatGPT WHAM usage: no quota windows or reset grants returned".into())
    } else {
        Ok(parsed)
    }
}

pub fn parse_opencode_go_usage(body: &Value, observed_at: DateTime<Utc>) -> Vec<QuotaWindow> {
    [
        ("rolling", "go_rolling", "Go · Session", QuotaWindowKind::Session, Some(300)),
        ("weekly", "go_weekly", "Go · Weekly", QuotaWindowKind::Weekly, Some(10080)),
        ("monthly", "go_monthly", "Go · Monthly", QuotaWindowKind::Monthly, None),
    ]
    .into_iter()
    .filter_map(|(key, id, label, kind, minutes)| {
        let raw = body["usage"].get(key)?;
        let mut window = percent_window(id, label, raw["percent"].as_f64().map(|n| n.clamp(0.0, 100.0)), observed_at);
        window.kind = kind;
        window.window_duration_mins = minutes;
        window.resets_at = iso(&raw["resetsAt"]);
        Some(window)
    })
    .collect()
}

/// Use a no-redirect HTTP client for authenticated provider probes.
pub async fn probe_opencode_go_usage(http: &Client, api_key: &str) -> Result<Vec<QuotaWindow>, String> {
    let body = probe_json(http.get("https://opencode.ai/zen/go/v1/usage").bearer_auth(api_key), "OpenCode Go usage").await?;
    let windows = parse_opencode_go_usage(&body, Utc::now());
    if windows.is_empty() { Err("OpenCode Go usage: no quota windows returned".into()) } else { Ok(windows) }
}

/// OpenCode Go key and/or Codex OAuth credentials explicitly owned by this adapter.
pub struct OpenCodeProvider {
    descriptor: ProviderDescriptor,
    go: Vec<AccountTokenFile>,
    chatgpt: Vec<AccountTokenFile>,
    http: Client,
}

impl OpenCodeProvider {
    pub fn new(id: &str, label: &str, go: Vec<AccountTokenFile>, chatgpt: Vec<AccountTokenFile>) -> PortResult<Self> {
        let mut descriptor = descriptor(id, label, "opencode", AdapterMode::Live);
        if !chatgpt.is_empty() {
            descriptor.evidence_kinds.insert(EvidenceKind::ResetGrants);
        }
        Ok(Self { descriptor, go, chatgpt, http: http_client()? })
    }

    async fn go_file(&self, account: &mut ModelAccount, path: &Path) -> Result<Vec<QuotaWindow>, String> {
        let contents = secret_file(path).await?;
        let json;
        let key = if contents.starts_with('{') {
            json = serde_json::from_str::<Value>(&contents).map_err(|_| "invalid OpenCode Go credential file".to_string())?;
            str_field(&json["opencode-go"]["key"])
                .or_else(|| str_field(&json["key"]))
                .ok_or("OpenCode Go credential file has no API key")?
        } else {
            &contents
        };
        let windows = probe_opencode_go_usage(&self.http, key).await?;
        account.credential_fingerprint = Some(fingerprint("opencode-go", key));
        Ok(windows)
    }

    async fn chatgpt_file(&self, account: &mut ModelAccount, path: &Path) -> Result<Vec<QuotaWindow>, String> {
        let json: Value = serde_json::from_str(&secret_file(path).await?).map_err(|_| "invalid ChatGPT credential file".to_string())?;
        let token = str_field(&json["tokens"]["access_token"])
            .or_else(|| str_field(&json["accessToken"]))
            .ok_or("ChatGPT credential file has no access token")?;
        let id = str_field(&json["tokens"]["account_id"]).or_else(|| str_field(&json["accountId"]));
        let (plan, windows, grant) = probe_chatgpt_wham_usage(&self.http, token, id).await?;
        account.plan = plan;
        account.reset_grant = grant;
        account.credential_fingerprint = id.map(|id| fingerprint("chatgpt-account", id));
        Ok(windows)
    }
}

#[async_trait]
impl ModelProvider for OpenCodeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }
    fn facets(&self) -> ProviderFacets<'_> {
        ProviderFacets::observation_only(self.descriptor.evidence_kinds.clone())
    }
    async fn collect(&self) -> PortResult<Option<ObservationBatch>> {
        let mut accounts = Vec::with_capacity(self.go.len() + self.chatgpt.len());
        for (file, go) in self.go.iter().map(|f| (f, true)).chain(self.chatgpt.iter().map(|f| (f, false))) {
            let mut account = account(&self.descriptor, &file.id, &file.label);
            let result = if go { self.go_file(&mut account, &file.path).await } else { self.chatgpt_file(&mut account, &file.path).await };
            record_probe(&mut account, result, Utc::now());
            accounts.push(account);
        }
        Ok(Some(batch(&self.descriptor, "opencode", accounts, Utc::now())))
    }
}
