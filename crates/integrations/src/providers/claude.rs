use super::*;
use async_trait::async_trait;
use domain::ports::{ModelProvider, ProviderFacets};

fn window_shape(window: &mut QuotaWindow) {
    let (kind, duration, scope) = match window.window_key.as_str() {
        "five_hour" => (QuotaWindowKind::Session, Some(300), None),
        "seven_day" => (QuotaWindowKind::Weekly, Some(10080), None),
        key if key.starts_with("seven_day_") => (QuotaWindowKind::ModelScoped, Some(10080), key.strip_prefix("seven_day_")),
        _ => (QuotaWindowKind::Custom, None, None),
    };
    window.kind = kind;
    window.scope_model = scope.map(str::to_owned);
    window.window_duration_mins = duration;
    window.cadence = match kind {
        QuotaWindowKind::Session => ResetCadence::RollingOnUse,
        QuotaWindowKind::Weekly | QuotaWindowKind::ModelScoped => ResetCadence::Fixed,
        _ => ResetCadence::Unknown,
    };
}

fn window_label(key: &str) -> String {
    match key {
        "five_hour" => "5-hour".into(),
        "seven_day" => "Weekly".into(),
        "seven_day_oauth_apps" => "OAuth apps · Weekly".into(),
        "seven_day_cowork" => "Cowork · Weekly".into(),
        _ => key.strip_prefix("seven_day_").map_or_else(|| key.replace('_', " "), |scope| format!("{} · Weekly", scope.replace('_', " "))),
    }
}

fn spend_window(key: &str, raw: &Value, utilization_scale: f64, at: DateTime<Utc>) -> QuotaWindow {
    fn money(raw: &Value) -> Option<(f64, &str)> {
        let exponent = uint(&raw["exponent"]).filter(|n| *n <= 18)?;
        Some((raw["amount_minor"].as_u64()? as f64 / 10_f64.powi(exponent as i32), str_field(&raw["currency"])?))
    }
    let used = money(&raw["used"]);
    let limit = money(&raw["limit"]).filter(|(_, currency)| used.is_none_or(|(_, used_currency)| used_currency == *currency));
    let enabled = raw["enabled"].as_bool().or_else(|| raw["is_enabled"].as_bool()).or_else(|| raw["isEnabled"].as_bool());
    let mut utilization = raw["utilization"].as_f64().map(|p| p * utilization_scale);
    if key == "spend" && raw.get("utilization").is_none() && enabled != Some(false) {
        utilization = used
            .zip(limit)
            .filter(|(_, (amount, _))| *amount > 0.0)
            .map(|((used, _), (limit, _))| used / limit * 100.0)
            .or_else(|| raw["percent"].as_f64());
    }
    let mut window = percent_window(key, if key == "spend" { "Spend" } else { "Extra usage" }, utilization, at);
    window.kind = QuotaWindowKind::Spend;
    window.unit = QuotaUnit::Currency;
    window.currency = used
        .or(limit)
        .map(|(_, code)| code)
        .or_else(|| str_field(&raw["currency"]))
        .or((key == "extra_usage").then_some("USD"))
        .map(str::to_owned);
    window.used_amount = used.map(|(amount, _)| amount).or_else(|| {
        ["used_minor_units", "used_minor", "used_credits", "usedCredits"].iter().find_map(|k| raw[k].as_f64()).map(|n| n / 100.0)
    });
    window.limit_amount = limit.map(|(amount, _)| amount).or_else(|| {
        ["limit_minor_units", "limit_minor", "monthly_limit", "monthlyLimit"].iter().find_map(|k| raw[k].as_f64()).map(|n| n / 100.0)
    });
    window.resets_at = iso(&raw["resets_at"]);
    if window.used_amount.is_some() || window.limit_amount.is_some() {
        window.observation_state = QuotaObservationState::Measured;
    }
    if enabled == Some(false) {
        window.status = QuotaStatus::Rejected;
    }
    window
}

pub fn parse_claude_oauth_usage(body: &Value, observed_at: DateTime<Utc>) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();
    for key in [
        "five_hour",
        "seven_day",
        "seven_day_opus",
        "seven_day_sonnet",
        "seven_day_oauth_apps",
        "seven_day_cowork",
        "seven_day_omelette",
        "iguana_necktie",
        "omelette_promotional",
    ] {
        if let Some(raw) = body.get(key) {
            let mut window = percent_window(key, &window_label(key), raw["utilization"].as_f64(), observed_at);
            window_shape(&mut window);
            window.resets_at = iso(&raw["resets_at"]);
            windows.push(window);
        }
    }
    for (raw, known_weekly) in
        rows(&body["model_scoped"]).iter().map(|raw| (raw, true)).chain(rows(&body["limits"]).iter().map(|raw| (raw, false)))
    {
        let model = raw.pointer("/scope/model").filter(|v| v.is_object()).unwrap_or(&raw["model"]);
        let label = str_field(&model["display_name"]).or_else(|| str_field(&raw["display_name"]));
        let Some(scope) = str_field(&model["id"]).or_else(|| str_field(&raw["model_id"])).or_else(|| str_field(&raw["model"])).or(label)
        else {
            continue;
        };
        let name = label.unwrap_or(scope);
        let name_lower = name.to_ascii_lowercase();
        let slug = name_lower
            .strip_prefix("claude ")
            .unwrap_or(&name_lower)
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("_");
        if slug.is_empty() {
            continue;
        }
        let weekly = known_weekly || matches!(raw["kind"].as_str(), Some("weekly_scoped" | "weekly"));
        let key = if weekly { format!("seven_day_{slug}") } else { format!("model_scoped_{slug}") };
        let mut window = percent_window(&key, name, raw["percent"].as_f64().or_else(|| raw["utilization"].as_f64()), observed_at);
        window_shape(&mut window);
        window.kind = QuotaWindowKind::ModelScoped;
        window.scope_model = Some(scope.into());
        window.resets_at = iso(&raw["resets_at"]);
        if let Some(existing) = windows.iter_mut().find(|w| w.window_key == key) {
            if existing.used_percent.is_none() && window.used_percent.is_some() {
                *existing = window;
            }
        } else {
            windows.push(window);
        }
    }
    for key in ["extra_usage", "spend"] {
        if body[key].is_object() {
            windows.push(spend_window(key, &body[key], 1.0, observed_at));
        }
    }
    windows
}

/// Use a no-redirect HTTP client for authenticated provider probes.
pub async fn probe_claude_oauth_usage(http: &Client, access_token: &str) -> Result<Vec<QuotaWindow>, String> {
    let body = probe_json(
        http.get("https://api.anthropic.com/api/oauth/usage").bearer_auth(access_token).header("anthropic-beta", "oauth-2025-04-20"),
        "Claude OAuth usage",
    )
    .await?;
    let windows = parse_claude_oauth_usage(&body, Utc::now());
    if windows.is_empty() { Err("Claude OAuth usage: no quota windows returned".into()) } else { Ok(windows) }
}

fn meridian_time(raw: &Value) -> Option<DateTime<Utc>> {
    iso(raw).or_else(|| DateTime::from_timestamp_millis(raw.as_i64()?))
}

fn meridian_reason(raw: &Value) -> Option<&'static str> {
    match raw.as_str()? {
        "no_token" => Some("no_token"),
        "not_oauth" => Some("not_oauth"),
        "rate_limited" => Some("rate_limited"),
        "upstream_error" => Some("upstream_error"),
        _ => Some("unknown quota error"),
    }
}

pub fn parse_meridian_quota_all(body: &Value, provider_id: &str, observed_at: DateTime<Utc>) -> Vec<ModelAccount> {
    let provider = descriptor(provider_id, "Claude", "claude", AdapterMode::Live);
    rows(&body["profiles"])
        .iter()
        .filter_map(|profile| {
            let id = str_field(&profile["id"])?;
            let mut account = account(&provider, id, str_field(&profile["label"]).unwrap_or(id));
            account.plan = str_field(&profile["type"]).map(str::to_owned);
            account.is_active = profile["isActive"].as_bool().unwrap_or_else(|| body["activeProfile"].as_str() == Some(id));
            let reason = meridian_reason(&profile["error"]).or_else(|| meridian_reason(&profile["failure"]["reason"]));
            let consecutive = profile["failure"]["consecutiveFailures"].as_u64();
            let unsupported = reason == Some("not_oauth");
            let last_good = profile.get("lastGood").filter(|v| v.is_object() && profile["fetchedAt"].is_null());
            let snapshot = last_good.unwrap_or(profile);
            let fetched_at = meridian_time(&snapshot["fetchedAt"]);
            let stale = profile["stale"].as_bool() == Some(true) || (fetched_at.is_some() && (reason.is_some() || last_good.is_some()));
            account.last_observed_at = fetched_at;
            account.enrollment_state = if fetched_at.is_some() {
                EnrollmentState::Verified
            } else if reason.is_some() && !unsupported {
                EnrollmentState::Error
            } else {
                EnrollmentState::Unobserved
            };
            account.status_message = reason
                .filter(|_| !unsupported)
                .map(|reason| consecutive.map_or_else(|| reason.into(), |n| format!("{reason} ({n} consecutive failures)")));
            account.status = AccountStatus::resolve(account.enabled, None, observed_at, false, account.status_message.is_some());
            account.windows = rows(&snapshot["windows"])
                .iter()
                .filter_map(|raw| {
                    let key = str_field(&raw["type"])?;
                    let mut window = percent_window(key, &window_label(key), raw["utilization"].as_f64().map(|p| p * 100.0), observed_at);
                    window_shape(&mut window);
                    window.resets_at = meridian_time(&raw["resetsAt"]);
                    Some(window)
                })
                .collect();
            if snapshot["extraUsage"].is_object() {
                account.windows.push(spend_window("extra_usage", &snapshot["extraUsage"], 100.0, observed_at));
            }
            if account.windows.is_empty() {
                let mut window = percent_window("quota", "Quota", None, observed_at);
                window.observation_state = if unsupported {
                    QuotaObservationState::Unsupported
                } else if reason.is_some() {
                    QuotaObservationState::ProbeFailed
                } else if fetched_at.is_some() {
                    QuotaObservationState::Unavailable
                } else {
                    QuotaObservationState::NeverObserved
                };
                account.windows.push(window);
            }
            for window in &mut account.windows {
                window.observed_at = fetched_at;
                window.stale = stale;
                window.error_reason = reason.map(str::to_owned);
                if stale && window.observation_state == QuotaObservationState::Measured {
                    window.observation_state = QuotaObservationState::StaleLastGood;
                }
            }
            Some(account)
        })
        .collect()
}

/// Use a no-redirect HTTP client to keep the management API key on this origin.
pub async fn probe_meridian_quota(http: &Client, base: &str, api_key: Option<&str>) -> Result<Vec<ModelAccount>, String> {
    let url = base_url(base).map_err(|e| e.to_string())?;
    read_meridian(http, &url, api_key, "claude").await
}

async fn read_meridian(http: &Client, base: &Url, api_key: Option<&str>, provider_id: &str) -> Result<Vec<ModelAccount>, String> {
    let mut request = http.get(endpoint(base, "/v1/usage/quota/all"));
    if let Some(key) = api_key {
        let mut value = reqwest::header::HeaderValue::from_str(key).map_err(|_| "invalid Meridian API key".to_string())?;
        value.set_sensitive(true);
        request = request.header("x-api-key", value);
    }
    let body = probe_json(request, "Meridian quota").await?;
    if !body["profiles"].is_array() {
        return Err("Meridian quota: missing profile list".into());
    }
    Ok(parse_meridian_quota_all(&body, provider_id, Utc::now()))
}

enum Source {
    Meridian { base: Url, api_key_file: Option<PathBuf> },
    Tokens(Vec<AccountTokenFile>),
}

pub struct ClaudeProvider {
    descriptor: ProviderDescriptor,
    source: Source,
    http: Client,
}

impl ClaudeProvider {
    pub fn from_meridian(id: &str, label: &str, base: &str, api_key_file: Option<PathBuf>) -> PortResult<Self> {
        Ok(Self {
            descriptor: descriptor(id, label, "claude", AdapterMode::Live),
            source: Source::Meridian { base: base_url(base)?, api_key_file },
            http: http_client()?,
        })
    }

    pub fn from_token_files(id: &str, label: &str, tokens: Vec<AccountTokenFile>) -> PortResult<Self> {
        Ok(Self { descriptor: descriptor(id, label, "claude", AdapterMode::Live), source: Source::Tokens(tokens), http: http_client()? })
    }

    async fn probe_file(&self, path: &Path) -> Result<Vec<QuotaWindow>, String> {
        let contents = secret_file(path).await?;
        let json;
        let token = if contents.starts_with('{') {
            json = serde_json::from_str::<Value>(&contents).map_err(|_| "invalid Claude credential file".to_string())?;
            str_field(&json["claudeAiOauth"]["accessToken"])
                .or_else(|| str_field(&json["access_token"]))
                .ok_or_else(|| "Claude credential file has no access token".to_string())?
        } else {
            &contents
        };
        probe_claude_oauth_usage(&self.http, token).await
    }
}

#[async_trait]
impl ModelProvider for ClaudeProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }
    fn facets(&self) -> ProviderFacets<'_> {
        ProviderFacets::observation_only(self.descriptor.evidence_kinds.clone())
    }

    async fn collect(&self) -> PortResult<Option<ObservationBatch>> {
        let accounts = match &self.source {
            Source::Meridian { base, api_key_file } => {
                let key = match api_key_file {
                    Some(path) => Some(secret_file(path).await.map_err(PortError::Unreachable)?),
                    None => None,
                };
                read_meridian(&self.http, base, key.as_deref(), &self.descriptor.id).await.map_err(PortError::Unreachable)?
            }
            Source::Tokens(files) => {
                let mut accounts = Vec::with_capacity(files.len());
                for file in files {
                    let mut account = account(&self.descriptor, &file.id, &file.label);
                    record_probe(&mut account, self.probe_file(&file.path).await, Utc::now());
                    accounts.push(account);
                }
                accounts
            }
        };
        Ok(Some(batch(&self.descriptor, "claude", accounts, Utc::now())))
    }
}
