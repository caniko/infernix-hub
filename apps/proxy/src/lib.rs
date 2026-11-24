pub mod clients;
pub mod config;
mod error_classify;
pub mod hub_sync;
mod management;
mod pool;
mod router;
pub mod verify;
pub mod worker;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use axum::{Router, extract::Request};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use config::Config;
use contracts::providers::{
    AccountError, AccountErrorKind, AccountErrorSource, ActivityExchangeIn, ProxyLogEntry, QuotaObservationState, QuotaStatus, QuotaWindow,
    ResetGrant,
};
use error_classify::{account_secrets, classify_oauth_error, classify_upstream_http_error, local_error, probe_error, safe_label};
pub use pool::Lane;
use pool::{Account, Pool, enforces_quota, quota_window_active};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify, RwLock};
use uuid::Uuid;

pub struct Proxy {
    config: Config,
    hub_sync_configured: bool,
    pub(crate) source_host: String,
    pub(crate) source_user: String,
    state: RwLock<ProxyState>,
    http: reqwest::Client,
    oauth_refresh: Mutex<()>,
    local_discovery: Mutex<Option<Instant>>,
    local_discovery_generation: AtomicU64,
    quota_actions: Mutex<()>,
    codex_keys: Mutex<CodexKeys>,
    google_tokens: Mutex<HashMap<[u8; 32], VerifiedGoogleToken>>,
    hub_sync_notify: Arc<Notify>,
    shutdown: Notify,
    management_mock: Option<reqwest::Url>,
}

struct VerifiedGoogleToken {
    expires: Instant,
    client_id: String,
    email: String,
}

struct ProxyState {
    pool: Pool,
    usage_refreshes: HashMap<String, Option<Instant>>,
    probe_generation: u64,
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
            hub_sync_configured: false,
            source_host,
            source_user,
            http,
            state: RwLock::new(ProxyState {
                pool,
                usage_refreshes: HashMap::new(),
                probe_generation: 0,
                activity: VecDeque::new(),
                logs: VecDeque::new(),
                captures: Captures::default(),
            }),
            oauth_refresh: Mutex::new(()),
            local_discovery: Mutex::new(None),
            local_discovery_generation: AtomicU64::new(0),
            quota_actions: Mutex::new(()),
            codex_keys: Mutex::new(CodexKeys::default()),
            google_tokens: Mutex::new(HashMap::new()),
            hub_sync_notify: Arc::new(Notify::new()),
            shutdown: Notify::new(),
            management_mock: None,
        })
    }

    pub fn with_hub_sync_configured(mut self, configured: bool) -> Self {
        self.hub_sync_configured = configured;
        self
    }

    pub(crate) fn set_mock_endpoints(&mut self, base: reqwest::Url) {
        self.management_mock = Some(base);
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

    fn notify_hub_sync(&self) {
        self.hub_sync_notify.notify_one();
    }

    async fn log(&self, level: &'static str, message: &str) {
        match level {
            "error" => tracing::error!(message = %message),
            "warn" => tracing::warn!(message = %message),
            _ => tracing::info!(message = %message),
        }
        let mut state = self.state.write().await;
        if state.logs.len() == LOG_CAP {
            state.logs.pop_front();
        }
        state.logs.push_back(ProxyLogEntry { at: Utc::now(), stream: "proxy".into(), level: level.into(), message: message.into() });
    }

    async fn log_account_error(&self, account: &Account, error: &AccountError) {
        let label = safe_label(account);
        tracing::warn!(
            account = %label, lane = account.lane.name(), kind = error.kind.as_str(), source = ?error.source,
            http_status = error.http_status, provider_code = error.provider_code.as_deref(), message = %error.message,
            "upstream account error"
        );
        self.log("warn", &format!("{} ({label}): {}", account.lane.name(), error.summary())).await;
    }

    async fn report_account_error(&self, account: &Account, error: &AccountError) {
        let changed = {
            let mut state = self.state.write().await;
            state
                .pool
                .accounts
                .get_mut(&account.id)
                .filter(|a| !a.disabled && a.same_secret(account))
                .is_some_and(|a| a.record_error_once(error))
        };
        if changed {
            self.notify_hub_sync();
        }
        self.log_account_error(account, error).await;
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

    pub async fn discover_local_commands(&self) {
        let _last = self.local_discovery.lock().await;
        self.discover_local_commands_locked().await;
        self.local_discovery_generation.fetch_add(1, Ordering::Release);
    }

    pub async fn refresh_local_commands(&self, min_interval: Duration) -> bool {
        let seen = self.local_discovery_generation.load(Ordering::Acquire);
        self.refresh_local_commands_since(min_interval, seen).await
    }

    async fn refresh_local_commands_since(&self, min_interval: Duration, seen: u64) -> bool {
        if !self.config.discover_local {
            return false;
        }
        let mut last = self.local_discovery.lock().await;
        if last.is_some_and(|at| at.elapsed() < min_interval) {
            // Reuse a pass completed since the caller's snapshot only while it is fresh.
            return self.local_discovery_generation.load(Ordering::Acquire) != seen;
        }
        self.discover_local_commands_locked().await;
        *last = Some(Instant::now());
        self.local_discovery_generation.fetch_add(1, Ordering::Release);
        true
    }

    async fn discover_local_commands_locked(&self) {
        if !self.config.discover_local {
            return;
        }
        use std::process::Stdio;
        use tokio::{process::Command, time::timeout};

        if let Some(raw) = read_local_opencode_auth().await {
            pool::add_opencode(&mut self.state.write().await.pool, &raw, &self.config.exclude_local_lanes);
        }

        if self.config.exclude_local_lanes.contains(&Lane::Google) {
            return;
        }
        let manifests = pool::provenance_google_manifests(&self.config, &self.source_user);
        if manifests.is_empty() {
            return;
        }
        let candidates: Vec<std::path::PathBuf> = match &self.config.provenance_oauth_bin {
            Some(bin) => vec![bin.clone()],
            None => vec!["provenance-oauth".into(), "/run/current-system/sw/bin/provenance-oauth".into()],
        };
        let env_vars: Vec<(&'static str, std::ffi::OsString)> = ["PATH", "HOME", "USER", "XDG_RUNTIME_DIR", "XDG_STATE_HOME"]
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (key, value)))
            .collect();
        let loaded = futures_util::future::join_all(manifests.into_iter().map(|(label, path)| {
            let candidates = &candidates;
            let env_vars = &env_vars;
            async move {
                for bin in candidates {
                    let read = Command::new(bin)
                        .arg("--config")
                        .arg(&path)
                        .arg("agy-token")
                        .env_clear()
                        .envs(env_vars.iter().map(|(k, v)| (*k, v)))
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .kill_on_drop(true)
                        .output();
                    match timeout(Duration::from_secs(5), read).await {
                        Ok(Ok(output)) if output.status.success() && output.stdout.len() <= 2 * 1024 * 1024 => {
                            if let Ok(raw) = serde_json::from_slice::<Value>(&output.stdout) {
                                return Some((label, raw));
                            }
                            return None;
                        }
                        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => continue,
                        _ => return None,
                    }
                }
                None
            }
        }))
        .await;
        let mut state = self.state.write().await;
        let revision = state.pool.revision;
        let before = state.pool.accounts.len();
        for (label, raw) in loaded.into_iter().flatten() {
            pool::add_provenance_google(&mut state.pool, &label, raw);
        }
        if state.pool.accounts.len() != before {
            state.pool.revision += 1;
        }
        if state.pool.revision != revision {
            self.notify_hub_sync();
        }
    }

    async fn google_tokeninfo(&self, token: &str) -> Result<Value, &'static str> {
        let tokeninfo_url = self
            .management_mock
            .as_ref()
            .and_then(|base| base.join("/test/google-tokeninfo").ok())
            .map_or_else(|| "https://oauth2.googleapis.com/tokeninfo".to_owned(), |url| url.to_string());
        provider_json(self.http.post(tokeninfo_url).form(&[("access_token", token)])).await
    }

    async fn google_token_email(&self, token: &str) -> Option<String> {
        let info = self.google_tokeninfo(token).await.ok()?;
        let verified = info["email_verified"] == true || info["email_verified"] == "true";
        verified.then(|| pool::text(&info, &["email"]).map(str::to_owned)).flatten()
    }

    async fn hydrate_google_metadata(&self) {
        self.hydrate_missing_google_metadata(true).await;
    }

    async fn hydrate_missing_google_metadata(&self, include_project: bool) {
        let missing: Vec<Account> = {
            let state = self.state.read().await;
            state
                .pool
                .accounts
                .values()
                .filter(|a| a.needs_google_metadata() && (include_project || (a.email.is_none() && a.google_client.is_some())))
                .cloned()
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let resolved = futures_util::future::join_all(missing.into_iter().map(|account| async move {
            let mut account = self.token_account(&account, false).await.ok()?;
            let base = account.base.clone();
            let lookup = async |access: &str, email, project| {
                tokio::join!(async { if email { self.google_token_email(access).await } else { None } }, async {
                    if project { self.google_project_id(base.as_ref(), access).await } else { None }
                },)
            };
            let need_email = account.email.is_none();
            let need_project = include_project && account.project.is_none();
            let (mut email, mut project) = lookup(&account.access, need_email, need_project).await;
            let retry_email = need_email && email.is_none();
            let retry_project = need_project && project.is_none();
            if (retry_email || retry_project)
                && account.refresh.is_some()
                && let Ok(fresh) = self.token_account(&account, true).await
                && fresh.access != account.access
            {
                account = fresh;
                let (found_email, found_project) = lookup(&account.access, retry_email, retry_project).await;
                email = email.or(found_email);
                project = project.or(found_project);
            }
            Some((account, email, project))
        }))
        .await;
        let mut state = self.state.write().await;
        for (account, email, project) in resolved.into_iter().flatten() {
            if let Some(current) = state.pool.accounts.get_mut(&account.id).filter(|c| !c.disabled && c.same_credential(&account)) {
                if current.email.is_none() {
                    current.email = email;
                }
                if project.is_some() && current.set_google_project(project) {
                    state.pool.revision += 1;
                }
            }
        }
    }

    async fn authorize_google(&self, token: &str) -> bool {
        if token.is_empty() || token.len() > 32_768 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return false;
        }
        let matches_pool = |pool: &Pool, client_id: &str, email: &str| {
            pool.accounts.values().any(|a| {
                !a.disabled
                    && !a.api_key
                    && a.lane == Lane::Google
                    && a.google_client.as_deref() == Some(client_id)
                    && a.email.as_deref().is_some_and(|e| e.eq_ignore_ascii_case(email))
            })
        };
        {
            let state = self.state.read().await;
            if state.pool.accounts.values().any(|a| {
                !a.disabled && !a.api_key && a.lane == Lane::Google && !a.access.is_empty() && config::matches_secret(&a.access, token)
            }) {
                return true;
            }
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        {
            let now = Instant::now();
            let mut cache = self.google_tokens.lock().await;
            cache.retain(|_, entry| entry.expires > now);
            if let Some(entry) = cache.get(&digest) {
                let client_id = entry.client_id.clone();
                let email = entry.email.clone();
                drop(cache);
                return matches_pool(&self.state.read().await.pool, &client_id, &email);
            }
        }
        // Remember the generation before reading the pool or awaiting tokeninfo.
        // An overlapping reload can finish discovery before its metadata is hydrated.
        let discovery_seen = self.local_discovery_generation.load(Ordering::Acquire);
        let reload_interval = Duration::from_secs(15);
        self.hydrate_missing_google_metadata(false).await;
        let has_pool_candidates = |pool: &Pool| {
            pool.accounts
                .values()
                .any(|a| !a.disabled && !a.api_key && a.lane == Lane::Google && a.google_client.is_some() && a.email.is_some())
        };
        let mut has_candidates = has_pool_candidates(&self.state.read().await.pool);
        let mut refreshed = false;
        if !has_candidates && self.refresh_local_commands_since(reload_interval, discovery_seen).await {
            refreshed = true;
            self.hydrate_missing_google_metadata(false).await;
            has_candidates = has_pool_candidates(&self.state.read().await.pool);
        }
        if !has_candidates {
            return false;
        }
        let Ok(info) = self.google_tokeninfo(token).await else {
            return false;
        };
        let verified = info["email_verified"] == true || info["email_verified"] == "true";
        let Some(email) = verified.then(|| pool::text(&info, &["email"])).flatten() else {
            return false;
        };
        let Some(expires_in) =
            info["expires_in"].as_u64().or_else(|| info["expires_in"].as_str().and_then(|s| s.parse::<u64>().ok())).filter(|n| *n > 0)
        else {
            return false;
        };
        let clients: Vec<&str> = [pool::text(&info, &["aud"]), pool::text(&info, &["azp"])].into_iter().flatten().collect();
        let mut matched_client = {
            let state = self.state.read().await;
            clients.iter().copied().find(|client_id| matches_pool(&state.pool, client_id, email)).map(str::to_owned)
        };
        if matched_client.is_none() && !refreshed && self.refresh_local_commands_since(reload_interval, discovery_seen).await {
            self.hydrate_missing_google_metadata(false).await;
            let state = self.state.read().await;
            matched_client = clients.iter().copied().find(|client_id| matches_pool(&state.pool, client_id, email)).map(str::to_owned);
        }
        let Some(client_id) = matched_client else {
            return false;
        };
        let now = Instant::now();
        let mut cache = self.google_tokens.lock().await;
        cache.retain(|_, entry| entry.expires > now);
        if cache.len() >= 256
            && !cache.contains_key(&digest)
            && let Some(oldest) = cache.iter().min_by_key(|(_, entry)| entry.expires).map(|(k, _)| *k)
        {
            cache.remove(&oldest);
        }
        cache.insert(
            digest,
            VerifiedGoogleToken { expires: now + Duration::from_secs(expires_in.clamp(1, 3600)), client_id, email: email.to_owned() },
        );
        true
    }

    async fn token(&self, a: &Account, force: bool) -> Result<String, Box<AccountError>> {
        self.token_account(a, force).await.map(|a| a.access)
    }

    async fn token_account(&self, a: &Account, force: bool) -> Result<Account, Box<AccountError>> {
        let auth_error = |message| {
            let kind = if force { AccountErrorKind::Other } else { AccountErrorKind::AuthExpired };
            Box::new(local_error(kind, AccountErrorSource::OAuthRefresh, message))
        };
        let superseded =
            || Box::new(local_error(AccountErrorKind::Other, AccountErrorSource::OAuthRefresh, "credential changed or was disabled"));
        if a.api_key || a.lane == Lane::Go {
            return if force { Err(auth_error("credential renewal is unavailable for this provider")) } else { Ok(a.clone()) };
        }
        let valid = |a: &Account| !a.access.is_empty() && a.expires.is_none_or(|expires| expires > Utc::now() + TimeDelta::seconds(30));
        if !force && valid(a) {
            return Ok(a.clone());
        }
        let _guard = self.oauth_refresh.lock().await;
        let current =
            self.state.read().await.pool.find(&a.id).filter(|c| c.same_credential(a) && !c.disabled).cloned().ok_or_else(superseded)?;
        if (!force || current.access != a.access) && valid(&current) {
            return Ok(current);
        }
        if matches!(a.lane, Lane::Claude | Lane::Codex) {
            let usable = |next: &Account| {
                next.lane == current.lane
                    && !next.api_key
                    && !next.disabled
                    && valid(next)
                    && (current.lane != Lane::Codex || codex_reload_matches(&current, next))
                    && next.organization.as_ref().zip(current.organization.as_ref()).is_none_or(|(x, y)| x == y)
            };
            let mut updated =
                if current.local { Pool::reload_local(&self.config, &current) } else { Pool::reload_file(&self.config, &current) }
                    .filter(usable);
            if current.local
                && self.config.discover_local
                && !self.config.exclude_local_lanes.contains(&current.lane)
                && updated.as_ref().is_none_or(|next| next.access == current.access)
                && let Some(raw) = read_local_opencode_auth().await
            {
                // Some OpenCode installs store OAuth only in their database.
                let mut found = Pool::default();
                pool::add_opencode(&mut found, &raw, &self.config.exclude_local_lanes);
                updated = found.accounts.remove(&current.id).filter(usable).or(updated);
            }
            let updated = updated.ok_or_else(|| auth_error("no usable credential found on reload"))?;
            let mut state = self.state.write().await;
            let entry =
                state.pool.accounts.get_mut(&a.id).filter(|entry| entry.same_secret(&current) && !entry.disabled).ok_or_else(superseded)?;
            if !entry.same_secret(&updated) && entry.error_generation == current.error_generation {
                let cleared = entry.clear_account_error() || entry.probe_failed;
                entry.probe_failed = false;
                if cleared {
                    self.notify_hub_sync();
                }
            }
            entry.replace_access(updated.access);
            entry.replace_refresh(updated.refresh);
            entry.codex_oauth_explicit = updated.codex_oauth_explicit;
            entry.expires = updated.expires;
            entry.source_fingerprint = updated.source_fingerprint;
            return Ok(entry.clone());
        }
        let (Some(refresh), Some(id), Some(secret)) = (&current.refresh, &current.google_client, &current.google_secret) else {
            return Err(auth_error("Google OAuth refresh fields unavailable"));
        };
        let token_url = self
            .management_mock
            .as_ref()
            .and_then(|base| base.join("/test/google-token").ok())
            .map_or_else(|| "https://oauth2.googleapis.com/token".to_owned(), |url| url.to_string());
        let refreshed = async {
            let response = self
                .http
                .post(token_url)
                .form(&[("grant_type", "refresh_token"), ("client_id", id), ("client_secret", secret), ("refresh_token", refresh)])
                .timeout(Duration::from_secs(12))
                .send()
                .await
                .map_err(|_| {
                    local_error(AccountErrorKind::UpstreamUnavailable, AccountErrorSource::OAuthRefresh, "Google OAuth request failed.")
                })?;
            let status = response.status();
            if !status.is_success() {
                let bytes = error_body(response).await;
                return Err(classify_oauth_error(Lane::Google, status, &bytes, &account_secrets(&current)));
            }
            let body = provider_response_json(response)
                .await
                .map_err(|message| local_error(AccountErrorKind::UpstreamUnavailable, AccountErrorSource::OAuthRefresh, message))?;
            let access = body["access_token"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 32_768 && s.bytes().all(|b| b.is_ascii_graphic()))
                .ok_or_else(|| {
                    local_error(AccountErrorKind::Other, AccountErrorSource::OAuthRefresh, "Google OAuth did not return a usable token.")
                })?
                .to_owned();
            Ok((access, body["expires_in"].as_i64().unwrap_or(3600).clamp(60, 86400)))
        }
        .await;
        let (access, expires_in) = match refreshed {
            Ok(result) => result,
            Err(error) => {
                self.report_account_error(&current, &error).await;
                return Err(Box::new(error));
            }
        };
        let mut state = self.state.write().await;
        let next = state.pool.accounts.get_mut(&a.id).filter(|n| n.same_secret(&current) && !n.disabled).ok_or_else(superseded)?;
        if next.access != access
            && next.error_generation == current.error_generation
            && next.last_error.as_ref().is_none_or(|err| !err.kind.is_blocking() || err.kind == AccountErrorKind::AuthExpired)
        {
            let cleared = next.clear_account_error() || next.probe_failed;
            next.probe_failed = false;
            if cleared {
                self.notify_hub_sync();
            }
        }
        next.replace_access(access);
        next.expires = Some(Utc::now() + TimeDelta::seconds(expires_in));
        Ok(next.clone())
    }

    async fn schedule_usage_refresh(self: &Arc<Self>, id: String) {
        const INTERVAL: Duration = Duration::from_secs(15);
        let started = Instant::now();
        let account = {
            let mut state = self.state.write().await;
            state.usage_refreshes.retain(|_, last| last.is_none_or(|at| started.saturating_duration_since(at) < INTERVAL));
            if state.usage_refreshes.contains_key(&id) {
                return;
            }
            let Some(account) = state.pool.accounts.get(&id).filter(|a| !a.disabled && !a.api_key).cloned() else { return };
            state.usage_refreshes.insert(id.clone(), None);
            account
        };
        let proxy = self.clone();
        tokio::spawn(async move {
            let _ = proxy.refresh_usage(&account).await;
            proxy.state.write().await.usage_refreshes.insert(id, Some(started));
        });
    }

    async fn oauth_usage_json(
        &self,
        account: &mut Account,
        request: impl Fn(&Account) -> reqwest::RequestBuilder,
    ) -> Result<Value, Box<AccountError>> {
        let send = |account: &Account| request(account).timeout(Duration::from_secs(12)).send();
        let mut response = send(account).await.map_err(|_| probe_transport_error())?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && !account.api_key {
            match self.token_account(account, true).await {
                Ok(fresh) if fresh.access != account.access => {
                    *account = fresh;
                    response = send(account).await.map_err(|_| probe_transport_error())?;
                }
                Err(error) => return Err(error),
                Ok(fresh) => {
                    *account = fresh;
                    return Err(unchanged_quota_credential());
                }
            }
        }
        quota_response_json(account, Ok(response)).await
    }

    fn antigravity_hosts(&self, base: Option<&reqwest::Url>) -> Vec<reqwest::Url> {
        if let Some(base) = base {
            return vec![base.clone()];
        }
        let mut hosts = vec![self.config.antigravity.clone()];
        if hosts[0].host_str() == Some("daily-cloudcode-pa.googleapis.com") {
            let mut standard = hosts[0].clone();
            if standard.set_host(Some("cloudcode-pa.googleapis.com")).is_ok() {
                hosts.push(standard);
            }
        }
        hosts
    }

    async fn google_project_id(&self, base: Option<&reqwest::Url>, token: &str) -> Option<String> {
        for mut url in self.antigravity_hosts(base) {
            url.set_path(&format!("{}/v1internal:loadCodeAssist", url.path().trim_end_matches('/')));
            let request = self
                .http
                .post(url)
                .bearer_auth(token)
                .header("user-agent", "antigravity/hub/2.24.0")
                .json(&serde_json::json!({"metadata":{"ideType":"ANTIGRAVITY"}}));
            if let Ok(raw) = provider_json(request).await
                && let Some(project) = pool::text(&raw, &["cloudaicompanionProject", "/cloudaicompanionProject/id"])
            {
                return Some(project.trim().to_owned());
            }
        }
        None
    }

    async fn google_quota_windows(
        &self,
        account: &mut Account,
        project: &mut Option<String>,
    ) -> Result<(Vec<QuotaWindow>, bool), Box<AccountError>> {
        use integrations::providers::antigravity;
        let hosts = self.antigravity_hosts(account.base.as_ref());
        let request = |url, access: &str, project: Option<&str>| {
            let body = project.map_or_else(|| serde_json::json!({}), |id| serde_json::json!({"project":id}));
            self.http
                .post(url)
                .bearer_auth(access)
                .header("user-agent", "antigravity/hub/2.24.0")
                .json(&body)
                .timeout(Duration::from_secs(12))
        };
        let mut renewed = false;
        let mut failure: Option<Box<AccountError>> = None;
        let mut received = false;
        for (op, parse) in [
            ("retrieveUserQuotaSummary", antigravity::parse_antigravity_summary as fn(&Value, _) -> _),
            ("fetchAvailableModels", antigravity::parse_antigravity_models_fallback),
        ] {
            let urls: Vec<_> = hosts
                .iter()
                .cloned()
                .map(|mut url| {
                    url.set_path(&format!("{}/v1internal:{op}", url.path().trim_end_matches('/')));
                    url
                })
                .collect();
            let mut windows = Vec::new();
            let mut pending = urls.clone();
            loop {
                let at = Utc::now();
                let reads = futures_util::future::join_all(pending.iter().map(|url| async {
                    quota_response_json(account, request(url.clone(), &account.access, project.as_deref()).send().await).await
                }))
                .await;
                let mut retry = Vec::new();
                for (url, response) in pending.into_iter().zip(reads) {
                    match response {
                        Ok(raw) => {
                            received = true;
                            windows.extend(parse(&raw, at));
                        }
                        Err(err)
                            if err.http_status == Some(401)
                                && err.kind == AccountErrorKind::AuthExpired
                                && !renewed
                                && account.refresh.is_some() =>
                        {
                            retry.push(url);
                        }
                        Err(err)
                            if matches!(
                                err.kind,
                                AccountErrorKind::VerificationRequired
                                    | AccountErrorKind::AccountSuspended
                                    | AccountErrorKind::AuthExpired
                                    | AccountErrorKind::BillingRequired
                            ) =>
                        {
                            return Err(err);
                        }
                        Err(err) => {
                            if failure.as_ref().is_none_or(|past| {
                                past.kind == AccountErrorKind::ProbeFailed || (err.kind.is_blocking() && !past.kind.is_blocking())
                            }) {
                                failure = Some(err);
                            }
                        }
                    }
                }
                if (project.is_some() || retry.is_empty()) && windows.iter().any(quota_evidence) {
                    return Ok((antigravity::merge_antigravity_windows(windows), op == "retrieveUserQuotaSummary"));
                }
                if retry.is_empty() {
                    break;
                }
                renewed = true;
                let fresh = match self.token_account(account, true).await {
                    Ok(fresh) if fresh.access != account.access => fresh,
                    Err(error) if error.kind.is_blocking() => return Err(error),
                    Err(error) => {
                        if failure.as_ref().is_none_or(|past| !past.kind.is_blocking()) {
                            failure = Some(error);
                        }
                        break;
                    }
                    Ok(_) => return Err(unchanged_quota_credential()),
                };
                *account = fresh;
                if project.is_none()
                    && let Some(discovered) = self.google_project_id(account.base.as_ref(), &account.access).await
                {
                    // Do not mix results from before and after discovering the project.
                    *project = Some(discovered);
                    windows.clear();
                    retry.clone_from(&urls);
                }
                pending = retry;
            }
        }
        Err(failure
            .filter(|err| !received || err.kind != AccountErrorKind::ProbeFailed)
            .unwrap_or_else(|| probe_error("provider returned no quota evidence")))
    }

    async fn refresh_usage(&self, account: &Account) -> Result<(), Box<AccountError>> {
        use integrations::providers::{claude, opencode};
        let mut account = {
            let mut state = self.state.write().await;
            // Allocate from one sequence so removing and re-enrolling an ID cannot reuse an in-flight generation.
            let last_generation = state.probe_generation;
            let current = state
                .pool
                .accounts
                .get_mut(&account.id)
                .filter(|c| !c.disabled && c.same_credential(account))
                .ok_or_else(|| probe_error("credential changed before quota refresh"))?;
            let generation = last_generation
                .max(current.probe_generation)
                .checked_add(1)
                .ok_or_else(|| probe_error("quota probe generation exhausted"))?;
            current.probe_generation = generation;
            let current = current.clone();
            state.probe_generation = generation;
            current
        };
        let generation = account.probe_generation;
        let error_generation = account.error_generation;
        let mut google_summary = false;
        let mut probed_project = None;
        let result = async {
            account = self.token_account(&account, false).await?;
            match account.lane {
                Lane::Claude => {
                    let mut url = self.config.anthropic.clone();
                    url.set_path("/api/oauth/usage");
                    url.set_query(Some("cedar_ember=1"));
                    let raw = self.oauth_usage_json(&mut account, |a| self.claude_oauth(self.http.get(url.clone()), &a.access)).await?;
                    let now = Utc::now();
                    let (grants, next) = claude_grants(&raw, now);
                    Ok((None, None, claude::parse_claude_oauth_usage(&raw, now), grants, next))
                }
                Lane::Codex => {
                    let url = self.wham("/backend-api/wham/usage");
                    let raw = self
                        .oauth_usage_json(&mut account, |a| openai_account(self.http.get(url.clone()).bearer_auth(&a.access), a))
                        .await?;
                    let (plan, windows, mut grant) = opencode::parse_chatgpt_wham_usage(&raw, Utc::now());
                    let credits = self.wham("/backend-api/wham/rate-limit-reset-credits");
                    let mut grants = Vec::new();
                    if let Ok(raw) = provider_json(openai_account(self.http.get(credits).bearer_auth(&account.access), &account)).await
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
                            || windows.iter().any(|w| w.status == QuotaStatus::Rejected || w.used_percent.is_some_and(|n| n >= 100.0))
                            || account.cooldown_until().is_some_and(|t| t > at);
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
                    Ok((plan, None, windows, grants, next))
                }
                Lane::Go => {
                    let url = router::upstream(self, &account, &axum::http::Uri::from_static("/v1/usage"))
                        .ok_or_else(|| probe_error("invalid OpenCode Go usage endpoint"))?;
                    let raw = self.oauth_usage_json(&mut account, |a| self.http.get(url.clone()).bearer_auth(&a.access)).await?;
                    Ok((None, None, opencode::parse_opencode_go_usage(&raw, Utc::now()), vec![], None))
                }
                Lane::Google => {
                    // A project discovered by this probe is not yet part of the enrolled route.
                    probed_project = match &account.project {
                        Some(project) => Some(project.clone()),
                        None => self.google_project_id(account.base.as_ref(), &account.access).await,
                    };
                    let (windows, summary) = self.google_quota_windows(&mut account, &mut probed_project).await?;
                    google_summary = summary;
                    let email = if account.email.is_none() { self.google_token_email(&account.access).await } else { None };
                    Ok((None, email, windows, vec![], None))
                }
            }
        }
        .await
        .and_then(|r| {
            if r.2.iter().any(quota_evidence) || !r.3.is_empty() { Ok(r) } else { Err(probe_error("provider returned no quota evidence")) }
        });
        let mut state = self.state.write().await;
        let current = state
            .pool
            .accounts
            .get_mut(&account.id)
            .filter(|c| !c.disabled && c.probe_generation == generation && c.same_secret(&account))
            .ok_or_else(|| probe_error("quota refresh was superseded"))?;
        let error_is_current = current.error_generation == error_generation;
        let was_probe_failed = current.probe_failed;
        let project_changed = probed_project.is_some() && current.set_google_project(probed_project);
        let changed;
        let result = match result {
            Ok((plan, email, windows, grants, next)) => {
                if plan.is_some() {
                    current.plan = plan;
                }
                if email.is_some() && current.email.is_none() {
                    current.email = email;
                }
                let now = Utc::now();
                merge_probe_windows(current, windows, google_summary, now);
                current.grants = grants;
                current.next_grant = next;
                current.observed = Some(now);
                current.probe_failed = false;
                changed = error_is_current && current.clear_account_error();
                Ok(())
            }
            Err(e) => {
                current.probe_failed = true;
                changed = error_is_current && current.record_error_once(&e);
                if current.lane == Lane::Google {
                    let now = Utc::now();
                    current.windows.retain(|w| {
                        let keep = w.resets_at.is_some() || quota_window_active(Lane::Google, w, now);
                        if !keep {
                            current.google_fallback_keys.remove(&w.window_key);
                        }
                        keep
                    });
                }
                for window in &mut current.windows {
                    window.stale = true;
                    window.observation_state = QuotaObservationState::StaleLastGood;
                    window.error_reason = Some(e.summary());
                }
                Err(e)
            }
        };
        state.pool.revision += i64::from(project_changed);
        drop(state);
        if project_changed || changed || was_probe_failed != result.is_err() {
            self.notify_hub_sync();
        }
        if let Err(error) = &result {
            self.log_account_error(&account, error).await;
        }
        result
    }

    fn claude_oauth(&self, request: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
        request
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("user-agent", CLAUDE_AGENT)
            .header("cache-control", "no-cache")
    }

    fn wham(&self, path: &str) -> reqwest::Url {
        let mut url = self.config.codex.clone();
        url.set_path(path);
        url
    }
}

fn quota_evidence(window: &QuotaWindow) -> bool {
    window.used_percent.is_some() || window.status == QuotaStatus::Rejected
}

fn codex_reload_matches(enrolled: &Account, next: &Account) -> bool {
    let basic = enrolled.chatgpt_user.as_ref().is_none_or(|user| next.chatgpt_user.as_ref() == Some(user))
        && enrolled.chatgpt_id.as_ref().is_none_or(|id| next.chatgpt_id.as_ref() == Some(id));
    if !basic {
        return false;
    }
    if next.access != enrolled.access {
        let claims = pool::jwt_claims(&next.access);
        let auth = &claims["https://api.openai.com/auth"];
        return enrolled.chatgpt_id.as_ref().is_none_or(|id| pool::text(auth, &["chatgpt_account_id"]) == Some(id.as_str()))
            && enrolled.chatgpt_user.as_ref().is_none_or(|user| pool::text(auth, &["chatgpt_user_id"]) == Some(user.as_str()));
    }
    true
}

fn google_quota_scope(w: &QuotaWindow) -> Option<&str> {
    ["gemini-argon", "gemini", "3p"]
        .into_iter()
        .find(|scope| w.window_key.strip_prefix(scope).is_some_and(|suffix| suffix.starts_with('-')))
        .or(w.scope_model.as_deref())
}

fn is_weekly_window(w: &QuotaWindow) -> bool {
    w.kind == contracts::providers::QuotaWindowKind::Weekly || w.window_key.ends_with("-weekly")
}

fn merge_probe_windows(account: &mut Account, windows: Vec<QuotaWindow>, google_summary: bool, now: DateTime<Utc>) {
    let google = account.lane == Lane::Google;
    let codex_recovered = account.lane == Lane::Codex && !windows.iter().any(|w| w.status == QuotaStatus::Rejected);
    let measured = |healthy_only: bool| {
        windows
            .iter()
            .filter(|w| quota_evidence(w) && (!healthy_only || !enforces_quota(w)))
            .filter_map(|w| google_quota_scope(w).map(|scope| (scope.to_owned(), is_weekly_window(w))))
            .collect::<BTreeSet<_>>()
    };
    let covered = if google_summary { measured(false) } else { BTreeSet::new() };
    let healthy = if google { measured(true) } else { BTreeSet::new() };
    let mut merged: BTreeMap<_, _> = windows.into_iter().map(|w| (w.window_key.clone(), w)).collect();
    let mut fallback = if google && !google_summary { merged.keys().cloned().collect() } else { BTreeSet::new() };
    for old in std::mem::take(&mut account.windows) {
        if codex_recovered && old.window_key == "rate_limit_denied" {
            continue;
        }
        let from_fallback = account.google_fallback_keys.contains(&old.window_key);
        let scope_cadence = google_quota_scope(&old).map(|scope| (scope.to_owned(), is_weekly_window(&old)));
        if quota_window_active(account.lane, &old, now)
            && !(from_fallback && scope_cadence.as_ref().is_some_and(|key| covered.contains(key)))
            && !(old.resets_at.is_none() && google && scope_cadence.as_ref().is_some_and(|key| healthy.contains(key)))
            && merged.get(&old.window_key).is_none_or(|new| quota_evidence(&old) && !quota_evidence(new))
        {
            if from_fallback {
                fallback.insert(old.window_key.clone());
            } else {
                fallback.remove(&old.window_key);
            }
            merged.insert(old.window_key.clone(), old);
        }
    }
    account.windows = merged.into_values().collect();
    account.google_fallback_keys = fallback;
}

async fn read_local_opencode_auth() -> Option<Value> {
    use std::process::Stdio;
    let read = tokio::process::Command::new("opencode")
        .args(["auth", "list", "--format", "json"])
        .env_clear()
        .envs(
            ["PATH", "HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "OPENCODE_DB"]
                .into_iter()
                .filter_map(|key| std::env::var_os(key).map(|value| (key, value))),
        )
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(3), read).await.ok()?.ok()?;
    if !output.status.success() || output.stdout.len() > 2 * 1024 * 1024 {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
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

async fn error_body(response: reqwest::Response) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(3), bounded_body(response, error_classify::ERROR_BODY_LIMIT))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn probe_transport_error() -> Box<AccountError> {
    Box::new(local_error(AccountErrorKind::UpstreamUnavailable, AccountErrorSource::QuotaProbe, "quota probe request failed"))
}

fn unchanged_quota_credential() -> Box<AccountError> {
    let mut error = local_error(
        AccountErrorKind::AuthExpired,
        AccountErrorSource::QuotaProbe,
        "No renewed credential was available after the quota endpoint returned 401.",
    );
    error.http_status = Some(401);
    Box::new(error)
}

async fn quota_response_json(account: &Account, response: Result<reqwest::Response, reqwest::Error>) -> Result<Value, Box<AccountError>> {
    let response = response.map_err(|_| probe_transport_error())?;
    let status = response.status();
    if status.is_success() {
        return provider_response_json(response).await.map_err(probe_error);
    }
    let headers = response.headers().clone();
    let bytes = error_body(response).await;
    Err(classify_upstream_http_error(account.lane, status, &headers, &bytes, AccountErrorSource::QuotaProbe, &account_secrets(account))
        .map(Box::new)
        .unwrap_or_else(|| probe_error("upstream rejected quota probe")))
}

async fn provider_json(request: reqwest::RequestBuilder) -> Result<Value, &'static str> {
    let response = request.timeout(Duration::from_secs(12)).send().await.map_err(|_| "upstream request failed")?;
    provider_response_json(response).await
}

async fn provider_response_json(response: reqwest::Response) -> Result<Value, &'static str> {
    if !response.status().is_success() {
        return Err("upstream rejected request");
    }
    serde_json::from_slice(&bounded_body(response, 2 * 1024 * 1024).await?).map_err(|_| "invalid upstream JSON")
}

#[cfg(test)]
mod tests;
