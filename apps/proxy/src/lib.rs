pub mod clients;
pub mod config;
pub mod hub_sync;
mod management;
mod pool;
mod router;

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use axum::{Router, extract::Request};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use config::Config;
use contracts::providers::{ActivityExchangeIn, ProxyLogEntry, QuotaObservationState, ResetGrant};
use pool::{Account, Lane, Pool};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, RwLock};
use uuid::Uuid;

pub struct Proxy {
    config: Config,
    pub(crate) source_host: String,
    pub(crate) source_user: String,
    state: RwLock<ProxyState>,
    http: reqwest::Client,
    oauth_refresh: Mutex<()>,
    quota_actions: Mutex<()>,
    codex_keys: Mutex<CodexKeys>,
    shutdown: Notify,
    #[cfg(test)]
    management_mock: Option<reqwest::Url>,
}

struct ProxyState {
    pool: Pool,
    activity: VecDeque<ActivityExchangeIn>,
    logs: VecDeque<ProxyLogEntry>,
    captures: Captures,
}

struct Captures {
    lease: Option<Instant>,
    lease_generation: u64,
    epoch: Uuid,
    sequence: u64,
    dropped: u64,
    records: VecDeque<Value>,
}

const ACTIVITY_CAP: usize = 1024;
const LOG_CAP: usize = 256;
const CAPTURE_CAP: usize = 96;
const CAPTURE_BYTES: usize = 64 * 1024;
const CLAUDE_AGENT: &str = "claude-code/2.1.273";

#[derive(Default)]
struct CodexKeys {
    fetched: Option<Instant>,
    attempted: Option<Instant>,
    jwks: Value,
}

impl Default for Captures {
    fn default() -> Self {
        Self { lease: None, lease_generation: 0, epoch: Uuid::new_v4(), sequence: 0, dropped: 0, records: VecDeque::new() }
    }
}

impl Captures {
    fn enabled(&self) -> bool {
        self.lease.is_some_and(|until| until > Instant::now())
    }

    fn push(&mut self, batch: Vec<Value>) {
        while self.records.len() + batch.len() > CAPTURE_CAP {
            let Some(oldest) = self.records.pop_front() else { break };
            let id = &oldest["id"];
            self.records.retain(|event| &event["id"] != id);
            self.dropped += 1;
        }
        for mut event in batch {
            self.sequence += 1;
            event["epoch"] = Value::String(self.epoch.to_string());
            event["sequence"] = Value::from(self.sequence);
            self.records.push_back(event);
        }
    }
}

impl Proxy {
    pub fn new(config: Config) -> Result<Self> {
        let (source_host, source_user) = hub_sync::source_identity();
        let mut pool = Pool::load(&config)?;
        pool.set_source(source_host.clone(), source_user.clone());
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self {
            config,
            source_host,
            source_user,
            http,
            state: RwLock::new(ProxyState { pool, activity: VecDeque::new(), logs: VecDeque::new(), captures: Captures::default() }),
            oauth_refresh: Mutex::new(()),
            quota_actions: Mutex::new(()),
            codex_keys: Mutex::new(CodexKeys::default()),
            shutdown: Notify::new(),
            #[cfg(test)]
            management_mock: None,
        })
    }

    pub fn app(self: Arc<Self>) -> Router {
        Router::new()
            .merge(router::routes())
            .nest("/v0/management", management::routes(self.clone(), false))
            .nest("/v8/management", management::routes(self.clone(), true))
            .with_state(self)
            .layer(
                tower_http::trace::TraceLayer::new_for_http()
                    .make_span_with(|request: &Request| tracing::debug_span!("proxy_http", method = %request.method()))
                    .on_failure(()),
            )
    }

    pub async fn shutdown_requested(&self) {
        self.shutdown.notified().await;
    }

    async fn log(&self, level: &'static str, message: &'static str) {
        let mut state = self.state.write().await;
        if state.logs.len() == LOG_CAP {
            state.logs.pop_front();
        }
        state.logs.push_back(ProxyLogEntry { at: Utc::now(), stream: "proxy".into(), level: level.into(), message: message.into() });
    }

    async fn authorize_codex(&self, token: &str) -> bool {
        let Some((signed, signature)) = token.rsplit_once('.').filter(|_| token.len() <= 32_768) else {
            return false;
        };
        let Some((header, payload)) = signed.split_once('.') else {
            return false;
        };
        let Some((header, claims)) = decode_jwt_segment(header).zip(decode_jwt_segment(payload)) else {
            return false;
        };
        let Some(kid) = pool::text(&header, &["kid"]).filter(|id| id.len() <= 256) else {
            return false;
        };
        const AUDIENCE: &str = "https://api.openai.com/v1";
        let aud = &claims["aud"];
        let login = &claims["https://api.openai.com/auth"];
        let Some((account, user)) = pool::text(login, &["chatgpt_account_id"]).zip(pool::text(login, &["chatgpt_user_id"])) else {
            return false;
        };
        if header["alg"] != "RS256"
            || claims["iss"] != "https://auth.openai.com"
            || !claims["exp"].as_i64().is_some_and(|exp| exp > Utc::now().timestamp())
            || !(aud == AUDIENCE || aud.as_array().is_some_and(|aud| aud.iter().any(|a| a == AUDIENCE)))
            || !self.state.read().await.pool.accounts.values().any(|a| {
                !a.disabled
                    && !a.api_key
                    && a.lane == Lane::Codex
                    && a.chatgpt_id.as_deref() == Some(account)
                    && a.chatgpt_user.as_deref() == Some(user)
            })
        {
            return false;
        }
        let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
            return false;
        };
        let mut keys = self.codex_keys.lock().await;
        let fresh = keys.fetched.is_some_and(|at| at.elapsed() < Duration::from_secs(3600));
        if (!fresh || jwk(&keys.jwks, kid).is_none()) && keys.attempted.is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
            keys.attempted = Some(Instant::now());
            if let Ok(jwks) = provider_json(self.http.get("https://auth.openai.com/.well-known/jwks.json")).await {
                keys.jwks = jwks;
                keys.fetched = Some(Instant::now());
            }
        }
        if keys.fetched.is_none_or(|at| at.elapsed() >= Duration::from_secs(3600)) {
            return false;
        }
        jwk(&keys.jwks, kid)
            .is_some_and(|key| key.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature).is_ok())
    }

    async fn token(&self, a: &Account, force: bool) -> Result<String> {
        if a.lane != Lane::Google || a.api_key {
            return if force { Err(anyhow!("credential renewal is unavailable for this provider")) } else { Ok(a.access.clone()) };
        }
        let valid = |a: &Account| !a.access.is_empty() && a.expires.is_none_or(|expires| expires > Utc::now() + TimeDelta::seconds(30));
        if !force && valid(a) {
            return Ok(a.access.clone());
        }
        let _guard = self.oauth_refresh.lock().await;
        let current = self
            .state
            .read()
            .await
            .pool
            .find(&a.id)
            .filter(|c| c.refresh == a.refresh && !c.disabled)
            .cloned()
            .ok_or_else(|| anyhow!("credential changed or was disabled"))?;
        if !force && valid(&current) {
            return Ok(current.access);
        }
        let (Some(refresh), Some(id), Some(secret)) = (&current.refresh, &current.google_client, &current.google_secret) else {
            return Err(anyhow!("Google OAuth refresh fields unavailable"));
        };
        let token_url = "https://oauth2.googleapis.com/token".to_owned();
        #[cfg(test)]
        let token_url =
            self.management_mock.as_ref().and_then(|base| base.join("/test/google-token").ok()).map_or(token_url, |url| url.to_string());
        let body = provider_json(self.http.post(token_url).form(&[
            ("grant_type", "refresh_token"),
            ("client_id", id),
            ("client_secret", secret),
            ("refresh_token", refresh),
        ]))
        .await
        .map_err(|_| anyhow!("Google OAuth refresh failed"))?;
        let access = body["access_token"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 32_768 && s.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or_else(|| anyhow!("Google OAuth did not return a usable token"))?
            .to_owned();
        let mut state = self.state.write().await;
        if let Some(next) = state.pool.accounts.get_mut(&a.id).filter(|n| n.refresh == a.refresh && !n.disabled) {
            next.access.clone_from(&access);
            next.expires = Some(Utc::now() + TimeDelta::seconds(body["expires_in"].as_i64().unwrap_or(3600).clamp(60, 86400)));
        } else {
            return Err(anyhow!("credential changed or was disabled"));
        }
        Ok(access)
    }

    async fn refresh_usage(&self, account: &Account) -> Result<(), &'static str> {
        use integrations::providers::{antigravity, claude, opencode};
        let result = async {
            let token = self.token(account, false).await.map_err(|_| "credential unavailable")?;
            match account.lane {
                Lane::Claude => {
                    let mut url = self.config.anthropic.clone();
                    url.set_path("/api/oauth/usage");
                    url.set_query(Some("cedar_ember=1"));
                    let raw = provider_json(self.claude_oauth(self.http.get(url), &token)).await?;
                    let now = Utc::now();
                    let (grants, next) = claude_grants(&raw, now);
                    Ok((None, claude::parse_claude_oauth_usage(&raw, now), grants, next))
                }
                Lane::Codex => {
                    let (plan, windows, mut grant) = if let Some(url) = self.probe_override(Lane::Codex, "/backend-api/wham/usage") {
                        let raw = provider_json(openai_account(self.http.get(url).bearer_auth(&token), account)).await?;
                        opencode::parse_chatgpt_wham_usage(&raw, Utc::now())
                    } else {
                        opencode::probe_chatgpt_wham_usage(&self.http, &token, account.chatgpt_id.as_deref())
                            .await
                            .map_err(|_| "Codex usage probe failed")?
                    };
                    let credits = self.wham("/backend-api/wham/rate-limit-reset-credits");
                    let mut grants = Vec::new();
                    if let Ok(raw) = provider_json(openai_account(self.http.get(credits).bearer_auth(&token), account)).await
                        && let Some(credits) = raw["credits"].as_array()
                    {
                        let at = Utc::now();
                        let mut usable = credits
                            .iter()
                            .filter_map(|row| {
                                if row["status"] != "available" || row["reset_type"] != "codex_rate_limits" {
                                    return None;
                                }
                                let id = row["id"].as_str()?;
                                let expiry = row["expires_at"].as_str()?.parse::<chrono::DateTime<Utc>>().ok()?;
                                (expiry > at).then_some((expiry, id))
                            })
                            .collect::<Vec<_>>();
                        usable.sort_unstable();
                        let applicable = grant.as_ref().is_some_and(|g| g.applicable)
                            || windows.iter().any(|w| w.used_percent.is_some_and(|n| n >= 100.0))
                            || account.cooldown.is_some_and(|t| t > at);
                        grants = usable
                            .into_iter()
                            .map(|(expiry, id)| ResetGrant {
                                grant_id: id.into(),
                                available_count: 1,
                                applicable,
                                checked_at: at,
                                expires_at: Some(expiry),
                                scope: Some("general".into()),
                                cleared_windows: vec![],
                                weekly_credit_back: false,
                                unavailable_reason: None,
                            })
                            .collect();
                        grant = None;
                    }
                    let next = grants.first().map(|g| g.grant_id.clone());
                    if grants.is_empty() {
                        grants.extend(grant);
                    }
                    Ok((plan, windows, grants, next))
                }
                Lane::Go => opencode::probe_opencode_go_usage(&self.http, &token)
                    .await
                    .map(|w| (None, w, vec![], None))
                    .map_err(|_| "OpenCode Go usage probe failed"),
                Lane::Google => {
                    let project = account.project.as_deref().map_or_else(|| serde_json::json!({}), |id| serde_json::json!({"project":id}));
                    let mut windows = Vec::new();
                    for (op, parse) in [
                        ("retrieveUserQuotaSummary", antigravity::parse_antigravity_summary as fn(&Value, _) -> _),
                        ("fetchAvailableModels", antigravity::parse_antigravity_models_fallback),
                    ] {
                        let mut url = self.config.antigravity.clone();
                        url.set_path(&format!("/v1internal:{op}"));
                        if let Ok(raw) = provider_json(
                            self.http.post(url).bearer_auth(&token).header("user-agent", "antigravity/hub/2.24.0").json(&project),
                        )
                        .await
                        {
                            windows = parse(&raw, Utc::now());
                            if windows.iter().any(|w: &contracts::providers::QuotaWindow| w.used_percent.is_some()) {
                                break;
                            }
                        }
                    }
                    Ok((None, windows, vec![], None))
                }
            }
        }
        .await
        .and_then(|r| if r.1.is_empty() && r.2.is_empty() { Err("provider returned no quota evidence") } else { Ok(r) });
        let mut state = self.state.write().await;
        let current = state
            .pool
            .accounts
            .get_mut(&account.id)
            .filter(|c| {
                c.lane == account.lane
                    && !c.disabled
                    && (c.access == account.access || (c.refresh.is_some() && c.refresh == account.refresh))
            })
            .ok_or("credential changed during quota refresh")?;
        match result {
            Ok((plan, windows, grants, next)) => {
                if plan.is_some() {
                    current.plan = plan;
                }
                current.windows = windows;
                current.grants = grants;
                current.next_grant = next;
                current.observed = Some(Utc::now());
                current.probe_failed = false;
                Ok(())
            }
            Err(e) => {
                current.probe_failed = true;
                for window in &mut current.windows {
                    window.stale = true;
                    window.observation_state = QuotaObservationState::StaleLastGood;
                    window.error_reason = Some(e.into());
                }
                Err(e)
            }
        }
    }

    fn claude_oauth(&self, request: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
        request
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("user-agent", CLAUDE_AGENT)
            .header("cache-control", "no-cache")
    }

    fn probe_override(&self, lane: Lane, path: &str) -> Option<reqwest::Url> {
        let mut url = match lane {
            Lane::Claude => self.config.anthropic.clone(),
            Lane::Codex => self.config.codex.clone(),
            _ => return None,
        };
        if matches!(url.host_str(), Some("api.anthropic.com" | "chatgpt.com")) {
            return None;
        }
        url.set_path(path);
        Some(url)
    }

    fn wham(&self, path: &str) -> reqwest::Url {
        self.probe_override(Lane::Codex, path).unwrap_or_else(|| {
            let mut url = self.config.codex.clone();
            url.set_path(path);
            url
        })
    }
}

fn decode_jwt_segment(segment: &str) -> Option<Value> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).ok()?).ok()
}

fn jwk(jwks: &Value, kid: &str) -> Option<ring::signature::RsaPublicKeyComponents<Vec<u8>>> {
    let key = jwks["keys"].as_array()?.iter().find(|key| key["kid"] == kid && key["kty"] == "RSA")?;
    Some(ring::signature::RsaPublicKeyComponents {
        n: URL_SAFE_NO_PAD.decode(key["n"].as_str()?).ok()?,
        e: URL_SAFE_NO_PAD.decode(key["e"].as_str()?).ok()?,
    })
}

fn claude_grants(raw: &Value, now: DateTime<Utc>) -> (Vec<ResetGrant>, Option<String>) {
    let block = &raw["cedar_ember"];
    if block["eligible"] != true {
        return (vec![], None);
    }
    let time = |v: &Value| v.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok());
    let cooling = time(&block["cooldown_until"]).is_some_and(|t| t > now);
    let grants = block["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|grant| {
            let id = pool::text(grant, &["id"])?;
            let available = u32::try_from(grant["resets_left"].as_u64()?).ok()?;
            let expires_at = time(&grant["ends_at"]);
            if available == 0 || expires_at.is_some_and(|t| t <= now) {
                return None;
            }
            let clears = grant["clears"].as_array()?.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
            let known = clears.iter().all(|c| matches!(*c, "five_hour" | "seven_day" | "seven_day_overage_included"));
            let weekly = grant["gives_back_weekly"].as_bool();
            let scope = match (known && clears.contains(&"seven_day_overage_included"), clears.contains(&"seven_day"), weekly) {
                (true, true, Some(false)) => "general",
                (true, false, Some(true)) => "fable",
                _ => "unknown",
            };
            let valid_date = |key| pool::text(grant, &[key]).is_none() || time(&grant[key]).is_some();
            let applicable = scope != "unknown"
                && !cooling
                && grant["usable_now"] == true
                && grant["paused"] != true
                && pool::text(grant, &["unavailable_reason"]).is_none()
                && valid_date("starts_at")
                && valid_date("ends_at")
                && time(&grant["starts_at"]).is_none_or(|t| t <= now);
            Some(ResetGrant {
                grant_id: id.into(),
                available_count: available,
                applicable,
                checked_at: now,
                expires_at,
                scope: Some(scope.into()),
                cleared_windows: clears.into_iter().map(str::to_owned).collect(),
                weekly_credit_back: weekly.unwrap_or_default(),
                unavailable_reason: (!applicable).then(|| "reset grant is not currently applicable".into()),
            })
        })
        .collect::<Vec<_>>();
    let next = pool::text(block, &["next_grant_id"]).map(str::to_owned);
    (grants, next)
}

fn openai_account(request: reqwest::RequestBuilder, account: &Account) -> reqwest::RequestBuilder {
    if let Some(id) = &account.chatgpt_id { request.header("chatgpt-account-id", id) } else { request }
}

async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "upstream body failed")? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err("upstream body too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn provider_json(request: reqwest::RequestBuilder) -> Result<Value, &'static str> {
    let response = request.timeout(Duration::from_secs(12)).send().await.map_err(|_| "upstream request failed")?;
    if !response.status().is_success() {
        return Err("upstream rejected request");
    }
    serde_json::from_slice(&bounded_body(response, 2 * 1024 * 1024).await?).map_err(|_| "invalid upstream JSON")
}

#[cfg(test)]
mod tests;
