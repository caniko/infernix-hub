use super::*;
use async_trait::async_trait;
use chrono::TimeDelta;
use domain::ports::{ModelProvider, ProviderFacets};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

fn worst(windows: impl IntoIterator<Item = QuotaWindow>) -> Vec<QuotaWindow> {
    let mut merged = BTreeMap::<String, QuotaWindow>::new();
    for window in windows {
        let existing = merged.entry(window.window_key.clone()).or_insert_with(|| window.clone());
        if window.used_percent > existing.used_percent
            || (window.used_percent == existing.used_percent && window.resets_at > existing.resets_at)
        {
            *existing = window;
        }
    }
    merged.into_values().collect()
}

fn quota(key: &str, label: &str, raw: &Value, at: DateTime<Utc>) -> QuotaWindow {
    let used = raw["remainingFraction"].as_f64().map(|n| (1.0 - n.clamp(0.0, 1.0)) * 100.0);
    let mut window = percent_window(key, label, used, at);
    window.resets_at = iso(&raw["resetTime"]);
    window
}

pub fn parse_antigravity_summary(body: &Value, requested_at: DateTime<Utc>) -> Vec<QuotaWindow> {
    let groups = rows(&body["groups"]).iter().chain(std::iter::once(body));
    worst(groups.flat_map(|group| {
        rows(&group["buckets"]).iter().filter_map(move |raw| {
            let key = str_field(&raw["bucketId"])?;
            if raw["disabled"].as_bool() == Some(true) {
                return None;
            }
            let group_name = str_field(&group["displayName"]);
            let family = if key.starts_with("gemini-argon-") {
                group_name.unwrap_or("Gemini Argon")
            } else if key.starts_with("gemini-") {
                "Gemini"
            } else if key.starts_with("3p-") {
                "Claude & GPT (shared)"
            } else {
                group_name.unwrap_or(key)
            };
            let (kind, duration, label) = match raw["window"].as_str() {
                Some("5h") => (QuotaWindowKind::Session, Some(300), "5-hour"),
                Some("weekly") => (QuotaWindowKind::Weekly, Some(10080), "Weekly"),
                _ => (QuotaWindowKind::Custom, None, str_field(&raw["displayName"]).unwrap_or("Quota")),
            };
            let mut window = quota(key, &format!("{family} · {label}"), raw, requested_at);
            window.kind = kind;
            window.scope_model = Some(family.into());
            window.window_duration_mins = duration;
            if let Some(minutes) = duration {
                if kind == QuotaWindowKind::Session {
                    window.cadence = ResetCadence::RollingOnUse;
                }
                if raw["remainingFraction"].as_f64() == Some(1.0)
                    && raw["description"].is_null()
                    && requested_at
                        .checked_add_signed(TimeDelta::minutes(i64::from(minutes)))
                        .zip(window.resets_at)
                        .is_some_and(|(expected, actual)| (actual - expected).abs() <= TimeDelta::seconds(60))
                {
                    window.resets_at = None;
                    window.cadence = ResetCadence::RollingOnUse;
                }
            }
            Some(window)
        })
    }))
}

pub fn parse_antigravity_models_fallback(body: &Value, observed_at: DateTime<Utc>) -> Vec<QuotaWindow> {
    worst(body["models"].as_object().into_iter().flatten().flat_map(|(id, model)| {
        let provider = model["modelProvider"].as_str();
        let family = if provider == Some("MODEL_PROVIDER_GOOGLE") || id.starts_with("gemini-") {
            "Gemini"
        } else if provider == Some("MODEL_PROVIDER_ANTHROPIC") || id.starts_with("claude-") {
            "Claude"
        } else if provider == Some("MODEL_PROVIDER_OPENAI") || id.starts_with("gpt-") {
            "GPT"
        } else {
            return Vec::new();
        };
        [
            ("quotaInfo", "quota", "Reported quota", QuotaWindowKind::Custom),
            ("weeklyQuotaInfo", "weekly", "Weekly", QuotaWindowKind::Weekly),
        ]
        .into_iter()
        .filter_map(|(key, suffix, label, kind)| {
            let raw = model.get(key)?;
            let mut window = quota(&format!("{}-{suffix}", family.to_lowercase()), &format!("{family} · {label}"), raw, observed_at);
            window.kind = kind;
            window.scope_model = Some(family.into());
            window.window_duration_mins = uint(&raw["windowDurationMins"]);
            Some(window)
        })
        .collect()
    }))
}

async fn cloud_quota(
    http: &Client,
    access_token: &str,
    project: &Value,
    operation: &str,
    parse: fn(&Value, DateTime<Utc>) -> Vec<QuotaWindow>,
) -> Vec<QuotaWindow> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let agent = format!("antigravity/hub/2.24.0 (aidev_client; os_type={platform}; arch={arch}; cl=963137146)");
    let agent = agent.as_str();
    let reads = ["daily-cloudcode-pa.googleapis.com", "cloudcode-pa.googleapis.com"].map(|host| async move {
        let requested_at = Utc::now();
        let request =
            http.post(format!("https://{host}/v1internal:{operation}")).bearer_auth(access_token).header("User-Agent", agent).json(project);
        match probe_json(request, "Antigravity quota").await {
            Ok(body) => parse(&body, requested_at),
            Err(_) => Vec::new(),
        }
    });
    let [daily, standard] = reads;
    let (daily, standard) = tokio::join!(daily, standard);
    worst(daily.into_iter().chain(standard))
}

/// Use a no-redirect HTTP client: the OAuth refresh sends credentials in the request body.
pub async fn probe_antigravity_quota(
    http: &Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
    project_id: Option<&str>,
) -> Result<(Option<String>, Vec<QuotaWindow>), String> {
    let token = probe_json(
        http.post("https://oauth2.googleapis.com/token").form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ]),
        "Google OAuth refresh",
    )
    .await?;
    let access = str_field(&token["access_token"]).ok_or("Google OAuth refresh: missing access token")?;
    let project = project_id.filter(|id| !id.is_empty()).map_or_else(|| json!({}), |id| json!({"project": id}));
    let identity = probe_json(http.get("https://www.googleapis.com/oauth2/v2/userinfo").bearer_auth(access), "Google account identity");
    let summary = cloud_quota(http, access, &project, "retrieveUserQuotaSummary", parse_antigravity_summary);
    let (identity, mut windows) = tokio::join!(identity, summary);
    if !windows.iter().any(|w| w.used_percent.is_some()) {
        let fallback = cloud_quota(http, access, &project, "fetchAvailableModels", parse_antigravity_models_fallback).await;
        if !fallback.is_empty() {
            windows = fallback;
        }
    }
    if windows.is_empty() {
        return Err("Antigravity quota: no quota windows returned".into());
    }
    let identity = identity.ok().and_then(|body| str_field(&body["id"]).map(|id| fingerprint("google-account", id)));
    Ok((identity, windows))
}

#[derive(Deserialize)]
struct Token {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    project_id: Option<String>,
}

pub struct AntigravityProvider {
    descriptor: ProviderDescriptor,
    tokens: Vec<AccountTokenFile>,
    http: Client,
}

impl AntigravityProvider {
    pub fn new(id: &str, label: &str, tokens: Vec<AccountTokenFile>) -> PortResult<Self> {
        Ok(Self { descriptor: descriptor(id, label, "antigravity", AdapterMode::Live), tokens, http: http_client()? })
    }

    async fn probe_file(&self, path: &Path) -> Result<(Option<String>, Vec<QuotaWindow>), String> {
        let token: Token =
            serde_json::from_str(&secret_file(path).await?).map_err(|_| "invalid Antigravity credential file".to_string())?;
        if [&token.client_id, &token.client_secret, &token.refresh_token].iter().any(|s| s.trim().is_empty()) {
            return Err("Antigravity credential file is missing OAuth fields".into());
        }
        probe_antigravity_quota(&self.http, &token.client_id, &token.client_secret, &token.refresh_token, token.project_id.as_deref()).await
    }
}

#[async_trait]
impl ModelProvider for AntigravityProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }
    fn facets(&self) -> ProviderFacets<'_> {
        ProviderFacets::observation_only(self.descriptor.evidence_kinds.clone())
    }
    async fn collect(&self) -> PortResult<Option<ObservationBatch>> {
        let mut accounts = Vec::with_capacity(self.tokens.len());
        for token in &self.tokens {
            let mut account = account(&self.descriptor, &token.id, &token.label);
            let result = self.probe_file(&token.path).await.map(|(fingerprint, windows)| {
                account.credential_fingerprint = fingerprint;
                windows
            });
            record_probe(&mut account, result, Utc::now());
            accounts.push(account);
        }
        Ok(Some(batch(&self.descriptor, "antigravity", accounts, Utc::now())))
    }
}
