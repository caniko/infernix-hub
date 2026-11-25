//! A separate, deny-by-default HTTP surface for Codex subscription workers.
use std::{
    collections::BTreeSet,
    fmt, fs,
    io::ErrorKind,
    net::SocketAddr,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use clap::Args;
use futures_util::{StreamExt, stream};
use reqwest::Url;
use serde::{
    Deserializer,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use tokio::{
    net::{TcpListener, UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};

use crate::{
    Proxy,
    config::{Config, matches_secret, private_dir, worker_secret_file},
    pool::{Lane, bounded_identifier},
    router::{
        LocalPoolResponse, POOL_QUOTA_HEADER, REQUEST_BODY_LIMIT, WorkerStreamOutcome, bearer, connection_headers, forward_authorized,
        lane_error,
    },
};

pub(crate) const CODEX_RESPONSES: &str = "/backend-api/codex/responses";
pub(crate) const GOOGLE_LOAD_CODE_ASSIST: &str = "/v1internal:loadCodeAssist";
pub(crate) const GOOGLE_FETCH_AVAILABLE_MODELS: &str = "/v1internal:fetchAvailableModels";
pub(crate) const GOOGLE_FETCH_USER_INFO: &str = "/v1internal:fetchUserInfo";
pub(crate) const GOOGLE_FETCH_ADMIN_CONTROLS: &str = "/v1internal:fetchAdminControls";
pub(crate) const GOOGLE_LIST_EXPERIMENTS: &str = "/v1internal:listExperiments";
pub(crate) const GOOGLE_RETRIEVE_QUOTA_SUMMARY: &str = "/v1internal:retrieveUserQuotaSummary";
pub(crate) const GOOGLE_COUNT_TOKENS: &str = "/v1internal:countTokens";
pub(crate) const GOOGLE_GENERATE_CONTENT: &str = "/v1internal:generateContent";
pub(crate) const GOOGLE_STREAM_GENERATE_CONTENT: &str = "/v1internal:streamGenerateContent";

pub(crate) const GOOGLE_CATALOG_RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
pub(crate) const GOOGLE_JSON_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const GOOGLE_SSE_FRAME_LIMIT: usize = 4 * 1024 * 1024;
const WORKER_DECODE_CONCURRENCY: usize = 16;
const WORKER_DECODE_WAIT: Duration = Duration::from_secs(5);
pub(crate) const WORKER_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NODES: usize = 250_000;

#[derive(Args, Default)]
pub struct WorkerOptions {
    /// Bind a worker socket in a private directory owned by the service user.
    #[arg(long, conflicts_with = "worker_listen")]
    pub worker_unix_socket: Option<PathBuf>,
    /// Bind the worker listener to a numeric loopback address.
    #[arg(long)]
    pub worker_listen: Option<SocketAddr>,
    /// Private file containing the dedicated worker bearer (required on TCP).
    #[arg(long)]
    pub worker_api_key_file: Option<PathBuf>,
    /// Exact Codex model names; comma-separated or repeated.
    #[arg(long, value_delimiter = ',')]
    pub worker_allowed_models: Vec<String>,
}

pub struct WorkerConfig {
    bind: Bind,
    scope: Scope,
}

enum Bind {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

struct Scope {
    key: Option<String>,
    models: BTreeSet<String>,
    upstream: Url,
    decode_permits: Arc<Semaphore>,
}

impl WorkerOptions {
    pub fn validate(self, shared: &Config) -> Result<Option<WorkerConfig>> {
        let bind = match (self.worker_unix_socket, self.worker_listen) {
            (None, None) => {
                ensure!(
                    self.worker_api_key_file.is_none() && self.worker_allowed_models.is_empty(),
                    "worker authentication and models require a worker listener"
                );
                return Ok(None);
            }
            (Some(path), None) => Bind::Unix(path),
            (None, Some(addr)) => {
                ensure!(addr.ip().is_loopback(), "worker TCP listener must use a loopback IP");
                Bind::Tcp(addr)
            }
            (Some(_), Some(_)) => bail!("configure only one worker listener"),
        };
        ensure!(!self.worker_allowed_models.is_empty(), "worker model allowlist must not be empty");
        for model in &self.worker_allowed_models {
            let lower = model.to_ascii_lowercase();
            ensure!(
                bounded_identifier(model) && !lower.contains("claude") && !lower.contains("anthropic"),
                "worker model allowlist must contain only non-empty Codex model names"
            );
        }
        let key = self.worker_api_key_file.as_deref().map(worker_secret_file).transpose()?;
        ensure!(!matches!(bind, Bind::Tcp(_)) || key.is_some(), "worker TCP listener requires a dedicated bearer");
        if let Some(key) = &key {
            ensure!(
                !matches_secret(key, &shared.client_key) && !matches_secret(key, &shared.management_key),
                "worker bearer must differ from the client and management keys"
            );
        }
        Ok(Some(WorkerConfig {
            bind,
            scope: Scope {
                key,
                models: self.worker_allowed_models.into_iter().collect(),
                upstream: Url::parse("https://chatgpt.com")?.join(CODEX_RESPONSES)?,
                decode_permits: Arc::new(Semaphore::new(WORKER_DECODE_CONCURRENCY)),
            },
        }))
    }
}

impl WorkerConfig {
    pub(crate) fn bearer(&self) -> Option<&str> {
        self.scope.key.as_deref()
    }

    pub fn validate_hub_token(&self, hub_token: &str) -> Result<()> {
        if let Some(key) = &self.scope.key {
            ensure!(!matches_secret(key, hub_token), "worker bearer must differ from hub token");
        }
        Ok(())
    }

    pub async fn validate_pool(&self, proxy: &Proxy) -> Result<()> {
        if let Some(key) = &self.scope.key {
            ensure!(!pool_has_secret(proxy, key).await, "worker bearer must differ from loaded provider credentials");
        }
        Ok(())
    }

    pub async fn bind(self) -> Result<BoundWorker> {
        let listener = match self.bind {
            Bind::Tcp(addr) => Listener::Tcp(TcpListener::bind(addr).await.context("bind worker TCP listener")?),
            Bind::Unix(path) => bind_unix(&path).await.with_context(|| format!("bind worker Unix socket {}", path.display()))?,
        };
        Ok(BoundWorker { listener, scope: self.scope })
    }

    #[cfg(test)]
    pub(crate) fn with_test_upstream(mut self, upstream: Url) -> Self {
        assert!(upstream.host_str().is_some_and(|host| host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())));
        self.scope.upstream = upstream;
        self
    }

    #[cfg(test)]
    pub(crate) fn app(self, proxy: Arc<Proxy>) -> Router {
        self.scope.app(proxy)
    }
}

pub struct BoundWorker {
    listener: Listener,
    scope: Scope,
}

enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener, UnixSocketGuard),
}

impl BoundWorker {
    #[cfg(test)]
    pub(crate) fn test_tcp_addr(&self) -> SocketAddr {
        let Listener::Tcp(listener) = &self.listener else { panic!("expected a worker TCP listener") };
        listener.local_addr().unwrap()
    }

    pub async fn serve(self, proxy: Arc<Proxy>, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
        let app = self.scope.app(proxy);
        match self.listener {
            Listener::Tcp(listener) => {
                tracing::info!(addr = %listener.local_addr()?, "scoped worker proxy listening");
                axum::serve(listener, app).with_graceful_shutdown(shutdown).await
            }
            Listener::Unix(listener, _guard) => {
                tracing::info!(addr = ?listener.local_addr()?, "scoped worker proxy listening");
                axum::serve(listener, app).with_graceful_shutdown(shutdown).await
            }
        }
    }
}

struct UnixSocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for UnixSocketGuard {
    fn drop(&mut self) {
        let Some(directory) = self.path.parent().and_then(|parent| fs::File::open(parent).ok()) else { return };
        // Never block the runtime while a start holds the lock across its probe.
        // If contended, leave the socket for a successor's stale-socket check.
        if directory.try_lock().is_err() {
            return;
        }
        if fs::symlink_metadata(&self.path).is_ok_and(|m| m.file_type().is_socket() && m.dev() == self.dev && m.ino() == self.ino) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn bind_unix(path: &Path) -> Result<Listener> {
    let path = std::path::absolute(path)?;
    ensure!(!path.components().any(|c| c == Component::ParentDir), "worker socket path cannot contain '..'");
    let parent = path.parent().context("worker socket requires a parent directory")?;
    let uid = rustix::process::geteuid().as_raw();
    // Check each ancestor before accessing the next. A sticky directory such as
    // /tmp is safe only when the selected child also belongs to us or root.
    let mut ancestors = parent.ancestors().collect::<Vec<_>>();
    while let Some(dir) = ancestors.pop() {
        match fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => private_dir(dir)?,
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(dir)?;
        let mode = metadata.mode();
        // In an unprivileged user namespace (such as a Nix build sandbox), `/`
        // belongs to unmapped host root (overflowuid) while still enforcing
        // directory and non-writable mode checks below.
        ensure!(
            metadata.is_dir() && (dir == Path::new("/") || metadata.uid() == 0 || metadata.uid() == uid),
            "worker socket ancestry must consist of trusted directories (no symlinks)"
        );
        ensure!(mode & 0o022 == 0 || mode & 0o1000 != 0, "worker socket ancestry is writable by other users");
        if dir == parent {
            ensure!(metadata.uid() == uid && mode & 0o777 == 0o700, "worker socket parent must be owned by the service user and mode 0700");
        }
    }
    // Serialize stale-socket checks and bind with other starts and socket cleanup.
    let directory = fs::File::open(parent)?;
    directory.try_lock().context("lock worker socket directory")?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(metadata.file_type().is_socket() && metadata.uid() == uid, "worker socket path is not an owned socket");
            match tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(&path)).await? {
                Err(error) if error.kind() == ErrorKind::ConnectionRefused => fs::remove_file(&path)?,
                Err(error) => return Err(error.into()),
                Ok(_) => bail!("worker socket is already in use"),
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // The private parent prevents access before chmod and before we serve requests.
    let listener = UnixListener::bind(&path)?;
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(metadata.file_type().is_socket() && metadata.uid() == uid, "worker socket path changed after bind");
    let guard = UnixSocketGuard { path, dev: metadata.dev(), ino: metadata.ino() };
    let permissions = fs::set_permissions(&guard.path, fs::Permissions::from_mode(0o600));
    drop(directory);
    permissions?;
    Ok(Listener::Unix(listener, guard))
}

impl Scope {
    fn app(self, proxy: Arc<Proxy>) -> Router {
        Router::new()
            .route(CODEX_RESPONSES, post(forward))
            .method_not_allowed_fallback(|| async { StatusCode::FORBIDDEN })
            .fallback(|| async { StatusCode::FORBIDDEN })
            .with_state((proxy, Arc::new(self)))
    }
}

// Only Codex inference parameters cross this boundary; unknown fields are not
// forwarded in case the upstream gives them routing or credential semantics.
const BODY_FIELDS: [&str; 26] = [
    "model",
    "input",
    "instructions",
    "stream",
    "store",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "text",
    "include",
    "previous_response_id",
    "conversation",
    "truncation",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "temperature",
    "top_p",
    "top_logprobs",
    "prompt_cache_key",
    "prompt_cache_retention",
    "safety_identifier",
    "service_tier",
    "user",
    "stream_options",
];

fn local_header(name: &str) -> bool {
    matches!(
        name,
        "accept-encoding"
            | "authorization"
            | "content-length"
            | "content-type"
            | "host"
            | "transfer-encoding"
            | "x-api-key"
            | "x-stainless-arch"
            | "x-stainless-lang"
            | "x-stainless-os"
            | "x-stainless-package-version"
            | "x-stainless-retry-count"
            | "x-stainless-runtime"
            | "x-stainless-runtime-version"
            | "x-stainless-timeout"
            | "b3"
            | "traceparent"
            | "x-opencode-client"
            | "x-opencode-project"
            | "x-opencode-session"
            | "x-opencode-session-id"
            | "x-session-affinity"
    )
}

fn inference_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "user-agent"
            | "openai-beta"
            | "originator"
            | "session_id"
            | "conversation_id"
            | "x-session-id"
            | "x-parent-session-id"
            | "x-client-request-id"
            | "x-client-version"
    )
}

struct JsonBudget {
    nodes: usize,
}

impl JsonBudget {
    fn new() -> Self {
        Self { nodes: 0 }
    }

    fn take<E: de::Error>(&mut self) -> Result<(), E> {
        self.nodes += 1;
        if self.nodes > MAX_JSON_NODES {
            return Err(de::Error::custom("JSON value exceeds node budget"));
        }
        Ok(())
    }
}

struct StrictSeed<'a> {
    depth: usize,
    budget: &'a mut JsonBudget,
}

impl<'de> DeserializeSeed<'de> for StrictSeed<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        if self.depth > MAX_JSON_DEPTH {
            return Err(de::Error::custom("JSON value exceeds nesting limit"));
        }
        self.budget.take()?;
        deserializer.deserialize_any(StrictJson { depth: self.depth, budget: self.budget })
    }
}

struct StrictJson<'a> {
    depth: usize,
    budget: &'a mut JsonBudget,
}

impl<'de> Visitor<'de> for StrictJson<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON without duplicate or reserved object keys")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value).map(Value::Number).ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(StrictSeed { depth: self.depth + 1, budget: self.budget })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            // Reject every serde_json private marker key at every depth.
            if key.starts_with("$serde_json::private::") || object.contains_key(&key) {
                return Err(de::Error::custom("duplicate or reserved JSON object key"));
            }
            let value = access.next_value_seed(StrictSeed { depth: self.depth + 1, budget: self.budget })?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

fn parse_strict_json_object(body: &[u8]) -> Result<Map<String, Value>, StatusCode> {
    if !body.trim_ascii_start().starts_with(b"{") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut json = serde_json::Deserializer::from_slice(body);
    let mut budget = JsonBudget::new();
    match (StrictSeed { depth: 0, budget: &mut budget }).deserialize(&mut json).and_then(|value| json.end().map(|()| value)) {
        Ok(Value::Object(object)) => Ok(object),
        Err(error) if error.is_data() => Err(StatusCode::FORBIDDEN),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

async fn pool_has_secret(proxy: &Proxy, key: &str) -> bool {
    proxy
        .state
        .read()
        .await
        .pool
        .accounts
        .values()
        .any(|account| account.current_secrets().into_iter().any(|secret| !secret.is_empty() && matches_secret(key, secret)))
}

async fn forward(State((proxy, scope)): State<(Arc<Proxy>, Arc<Scope>)>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    if parts.headers.get_all(header::CONNECTION).iter().any(|value| value.to_str().is_err()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(nominated) = connection_headers(&parts.headers)
        .into_iter()
        .filter(|name| !name.is_empty())
        .map(str::parse::<HeaderName>)
        .collect::<Result<Vec<_>, _>>()
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    for name in nominated {
        parts.headers.remove(name);
    }
    parts.headers.remove(header::CONNECTION);
    if let Some(key) = &scope.key
        && (parts.headers.get_all(header::AUTHORIZATION).iter().count() != 1
            || !bearer(&parts.headers).is_some_and(|token| matches_secret(token, key))
            || pool_has_secret(&proxy, key).await)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if parts.uri.query().is_some()
        || parts.uri.authority().is_some()
        || parts.headers.keys().any(|name| !local_header(name.as_str()) && !inference_header(name.as_str()))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(Ok(permit)) = tokio::time::timeout(WORKER_DECODE_WAIT, scope.decode_permits.clone().acquire_owned()).await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(body) = tokio::time::timeout(WORKER_BODY_READ_TIMEOUT, to_bytes(body, REQUEST_BODY_LIMIT)).await else {
        return StatusCode::REQUEST_TIMEOUT.into_response();
    };
    let Ok(body) = body else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let object = match parse_strict_json_object(&body) {
        Ok(object) => object,
        Err(status) => return status.into_response(),
    };
    if object.keys().any(|key| !BODY_FIELDS.contains(&key.as_str()))
        || !object.get("model").and_then(Value::as_str).is_some_and(|model| scope.models.contains(model))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(body) = serde_json::to_vec(&object) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    // Do not pass inbound credentials, transport headers, or client telemetry
    // to the shared forwarding path.
    parts.headers = parts
        .headers
        .iter()
        .filter(|(name, _)| inference_header(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    parts.headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    let response = forward_authorized(proxy, parts, body.into(), Value::Object(object), Lane::Codex, Some(&scope.upstream)).await;
    drop(permit);
    response
}

#[derive(Args, Default)]
pub struct GoogleWorkerOptions {
    /// Bind a Google/Antigravity worker socket in a private directory owned by the service user.
    #[arg(long, conflicts_with = "google_worker_listen")]
    pub google_worker_unix_socket: Option<PathBuf>,
    /// Bind the Google/Antigravity worker listener to a numeric loopback address.
    #[arg(long)]
    pub google_worker_listen: Option<SocketAddr>,
    /// Private file containing the dedicated Google worker bearer (required on TCP).
    #[arg(long)]
    pub google_worker_api_key_file: Option<PathBuf>,
    /// Exact Gemini model names; comma-separated or repeated.
    #[arg(long, value_delimiter = ',')]
    pub google_worker_allowed_models: Vec<String>,
}

pub struct GoogleWorkerConfig {
    bind: Bind,
    scope: GoogleScope,
}

struct GoogleScope {
    key: Option<String>,
    models: BTreeSet<String>,
    upstream_base: Url,
    decode_permits: Arc<Semaphore>,
}

impl GoogleWorkerOptions {
    pub fn validate(self, shared: &Config, codex: Option<&WorkerConfig>) -> Result<Option<GoogleWorkerConfig>> {
        let bind = match (self.google_worker_unix_socket, self.google_worker_listen) {
            (None, None) => {
                ensure!(
                    self.google_worker_api_key_file.is_none() && self.google_worker_allowed_models.is_empty(),
                    "Google worker authentication and models require a Google worker listener"
                );
                return Ok(None);
            }
            (Some(path), None) => Bind::Unix(path),
            (None, Some(addr)) => {
                ensure!(addr.ip().is_loopback(), "Google worker TCP listener must use a loopback IP");
                Bind::Tcp(addr)
            }
            (Some(_), Some(_)) => bail!("configure only one Google worker listener"),
        };
        if let Some(codex) = codex {
            match (&bind, &codex.bind) {
                (Bind::Unix(google_path), Bind::Unix(codex_path)) => {
                    ensure!(
                        std::path::absolute(google_path)? != std::path::absolute(codex_path)?,
                        "Google worker socket must differ from Codex worker socket"
                    );
                }
                (Bind::Tcp(google_addr), Bind::Tcp(codex_addr)) => {
                    ensure!(google_addr != codex_addr, "Google worker TCP listener must differ from Codex worker TCP listener");
                }
                _ => {}
            }
        }
        ensure!(!self.google_worker_allowed_models.is_empty(), "Google worker model allowlist must not be empty");
        for model in &self.google_worker_allowed_models {
            let lower = model.to_ascii_lowercase();
            ensure!(
                bounded_identifier(model)
                    && lower.starts_with("gemini-")
                    && !lower.contains("claude")
                    && !lower.contains("anthropic")
                    && !lower.contains("gpt")
                    && !lower.contains("openai"),
                "Google worker model allowlist must contain only non-empty Gemini model names"
            );
        }
        let key = self.google_worker_api_key_file.as_deref().map(worker_secret_file).transpose()?;
        ensure!(!matches!(bind, Bind::Tcp(_)) || key.is_some(), "Google worker TCP listener requires a dedicated bearer");
        if let Some(key) = &key {
            ensure!(
                !matches_secret(key, &shared.client_key)
                    && !matches_secret(key, &shared.management_key)
                    && !codex.and_then(WorkerConfig::bearer).is_some_and(|other| matches_secret(key, other)),
                "Google worker bearer must differ from client, management, and Codex worker keys"
            );
        }
        Ok(Some(GoogleWorkerConfig {
            bind,
            scope: GoogleScope {
                key,
                models: self.google_worker_allowed_models.into_iter().collect(),
                upstream_base: Url::parse("https://daily-cloudcode-pa.googleapis.com")?,
                decode_permits: Arc::new(Semaphore::new(WORKER_DECODE_CONCURRENCY)),
            },
        }))
    }
}

impl GoogleWorkerConfig {
    pub(crate) fn bearer(&self) -> Option<&str> {
        self.scope.key.as_deref()
    }

    pub fn validate_hub_token(&self, hub_token: &str) -> Result<()> {
        if let Some(key) = &self.scope.key {
            ensure!(!matches_secret(key, hub_token), "Google worker bearer must differ from hub token");
        }
        Ok(())
    }

    pub async fn validate_pool(&self, proxy: &Proxy) -> Result<()> {
        if let Some(key) = &self.scope.key {
            ensure!(!pool_has_secret(proxy, key).await, "Google worker bearer must differ from loaded provider credentials");
        }
        Ok(())
    }

    pub async fn bind(self) -> Result<BoundGoogleWorker> {
        let listener = match self.bind {
            Bind::Tcp(addr) => Listener::Tcp(TcpListener::bind(addr).await.context("bind Google worker TCP listener")?),
            Bind::Unix(path) => bind_unix(&path).await.with_context(|| format!("bind Google worker Unix socket {}", path.display()))?,
        };
        Ok(BoundGoogleWorker { listener, scope: self.scope })
    }

    #[cfg(test)]
    pub(crate) fn with_test_upstream(mut self, upstream_base: Url) -> Self {
        assert!(upstream_base.host_str().is_some_and(|host| host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())));
        self.scope.upstream_base = upstream_base;
        self
    }

    #[cfg(test)]
    pub(crate) fn app(self, proxy: Arc<Proxy>) -> Router {
        self.scope.app(proxy)
    }
}

pub struct BoundGoogleWorker {
    listener: Listener,
    scope: GoogleScope,
}

impl BoundGoogleWorker {
    #[cfg(test)]
    pub(crate) fn test_tcp_addr(&self) -> SocketAddr {
        let Listener::Tcp(listener) = &self.listener else { panic!("expected a Google worker TCP listener") };
        listener.local_addr().unwrap()
    }

    pub async fn serve(self, proxy: Arc<Proxy>, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
        let app = self.scope.app(proxy);
        match self.listener {
            Listener::Tcp(listener) => {
                tracing::info!(addr = %listener.local_addr()?, "scoped Google worker proxy listening");
                axum::serve(listener, app).with_graceful_shutdown(shutdown).await
            }
            Listener::Unix(listener, _guard) => {
                tracing::info!(addr = ?listener.local_addr()?, "scoped Google worker proxy listening");
                axum::serve(listener, app).with_graceful_shutdown(shutdown).await
            }
        }
    }
}

impl GoogleScope {
    fn app(self, proxy: Arc<Proxy>) -> Router {
        Router::new()
            .route(GOOGLE_LOAD_CODE_ASSIST, post(google_load_code_assist))
            .route(GOOGLE_FETCH_USER_INFO, post(google_metadata_stub))
            .route(GOOGLE_FETCH_ADMIN_CONTROLS, post(google_metadata_stub))
            .route(GOOGLE_LIST_EXPERIMENTS, post(google_metadata_stub))
            .route(GOOGLE_RETRIEVE_QUOTA_SUMMARY, post(google_metadata_stub))
            .route(GOOGLE_FETCH_AVAILABLE_MODELS, post(google_fetch_available_models))
            .route(GOOGLE_COUNT_TOKENS, post(google_count_tokens))
            .route(GOOGLE_GENERATE_CONTENT, post(google_generate_content))
            .route(GOOGLE_STREAM_GENERATE_CONTENT, post(google_stream_generate_content))
            .method_not_allowed_fallback(|| async { StatusCode::FORBIDDEN })
            .fallback(|| async { StatusCode::FORBIDDEN })
            .with_state((proxy, Arc::new(self)))
    }
}

const GOOGLE_LOAD_FIELDS: [&str; 2] = ["metadata", "cloudaicompanionProject"];
const GOOGLE_META_FIELDS: [&str; 2] = ["project", "metadata"];
const GOOGLE_COUNT_FIELDS: [&str; 5] = ["model", "project", "request", "requestId", "userAgent"];
const GOOGLE_GENERATE_FIELDS: [&str; 6] = ["model", "project", "request", "requestId", "requestType", "userAgent"];

const GOOGLE_COUNT_REQUEST_FIELDS: [&str; 9] = [
    "model",
    "contents",
    "systemInstruction",
    "system_instruction",
    "tools",
    "toolConfig",
    "tool_config",
    "generationConfig",
    "generation_config",
];

const GOOGLE_GENERATE_REQUEST_FIELDS: [&str; 13] = [
    "contents",
    "systemInstruction",
    "system_instruction",
    "tools",
    "toolConfig",
    "tool_config",
    "safetySettings",
    "safety_settings",
    "generationConfig",
    "generation_config",
    "labels",
    "sessionId",
    "session_id",
];

fn google_local_header(name: &str) -> bool {
    matches!(name, "accept-encoding" | "authorization" | "content-length" | "content-type" | "host" | "transfer-encoding" | "x-api-key")
}

fn google_inference_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "user-agent"
            | "client-metadata"
            | "x-goog-api-client"
            | "x-goog-api-format-version"
            | "x-client-version"
            | "x-client-request-id"
            | "x-session-id"
            | "session_id"
            | "conversation_id"
    )
}

fn is_forbidden_google_key(key: &str) -> bool {
    matches!(
        key,
        "cachedContent" | "cached_content" | "routingConfig" | "routing_config" | "modelSelectionConfig" | "model_selection_config"
    )
}

fn value_has_forbidden_google_key(value: &Value) -> bool {
    match value {
        Value::Object(map) => map_has_forbidden_google_key(map),
        Value::Array(items) => items.iter().any(value_has_forbidden_google_key),
        _ => false,
    }
}

fn map_has_forbidden_google_key(map: &Map<String, Value>) -> bool {
    map.iter().any(|(key, child)| is_forbidden_google_key(key.as_str()) || value_has_forbidden_google_key(child))
}

async fn prepare_google_request(
    proxy: &Proxy,
    scope: &GoogleScope,
    request: Request,
    allow_sse_query: bool,
    allowed_fields: &[&str],
) -> Result<(axum::http::request::Parts, Map<String, Value>, Option<String>, OwnedSemaphorePermit), StatusCode> {
    let (mut parts, body) = request.into_parts();
    if parts.headers.get_all(header::CONNECTION).iter().any(|value| value.to_str().is_err()) {
        return Err(StatusCode::FORBIDDEN);
    }
    let Ok(nominated) = connection_headers(&parts.headers)
        .into_iter()
        .filter(|name| !name.is_empty())
        .map(str::parse::<HeaderName>)
        .collect::<Result<Vec<_>, _>>()
    else {
        return Err(StatusCode::FORBIDDEN);
    };
    for name in nominated {
        parts.headers.remove(name);
    }
    parts.headers.remove(header::CONNECTION);
    if let Some(key) = &scope.key
        && (parts.headers.get_all(header::AUTHORIZATION).iter().count() != 1
            || !bearer(&parts.headers).is_some_and(|token| matches_secret(token, key))
            || pool_has_secret(proxy, key).await)
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let query = match parts.uri.query() {
        None => None,
        Some("alt=sse") if allow_sse_query => Some("alt=sse".to_owned()),
        Some(_) => return Err(StatusCode::FORBIDDEN),
    };
    if parts.uri.authority().is_some()
        || parts.headers.keys().any(|name| !google_local_header(name.as_str()) && !google_inference_header(name.as_str()))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let Ok(Ok(permit)) = tokio::time::timeout(WORKER_DECODE_WAIT, scope.decode_permits.clone().acquire_owned()).await else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let Ok(body) = tokio::time::timeout(WORKER_BODY_READ_TIMEOUT, to_bytes(body, REQUEST_BODY_LIMIT)).await else {
        return Err(StatusCode::REQUEST_TIMEOUT);
    };
    let Ok(body) = body else {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let object = parse_strict_json_object(&body)?;
    if object.keys().any(|key| !allowed_fields.contains(&key.as_str())) {
        return Err(StatusCode::FORBIDDEN);
    }
    parts.headers = parts
        .headers
        .iter()
        .filter(|(name, _)| google_inference_header(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    parts.headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    Ok((parts, object, query, permit))
}

async fn google_load_code_assist(State((proxy, scope)): State<(Arc<Proxy>, Arc<GoogleScope>)>, request: Request) -> Response {
    if let Err(status) = prepare_google_request(&proxy, &scope, request, false, &GOOGLE_LOAD_FIELDS).await {
        return status.into_response();
    }
    axum::Json(serde_json::json!({
        "cloudaicompanionProject": "worker-project",
        "currentTier": {"id": "standard-tier", "name": "Standard"},
        "allowedTiers": [{"id": "standard-tier", "isDefault": true}],
    }))
    .into_response()
}

async fn google_metadata_stub(State((proxy, scope)): State<(Arc<Proxy>, Arc<GoogleScope>)>, request: Request) -> Response {
    if let Err(status) = prepare_google_request(&proxy, &scope, request, false, &GOOGLE_META_FIELDS).await {
        return status.into_response();
    }
    axum::Json(serde_json::json!({})).into_response()
}

fn sanitize_google_worker_error(response: Response) -> Response {
    let status = response.status();
    let is_local_pool = response.extensions().get::<LocalPoolResponse>().is_some();
    let pool_quota = is_local_pool.then(|| response.headers().get(POOL_QUOTA_HEADER).cloned()).flatten();
    let retry_after = is_local_pool.then(|| response.headers().get(header::RETRY_AFTER).cloned()).flatten();
    let message = match status {
        StatusCode::UNAUTHORIZED => "worker authentication failed",
        StatusCode::FORBIDDEN => "worker request denied",
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNSUPPORTED_MEDIA_TYPE => "invalid worker request",
        StatusCode::TOO_MANY_REQUESTS => "upstream subscription quota exhausted",
        StatusCode::GATEWAY_TIMEOUT => "upstream inference timed out",
        StatusCode::SERVICE_UNAVAILABLE => "no eligible subscription account available",
        _ => "upstream inference failed",
    };
    let mut sanitized = lane_error(Lane::Google, status, message);
    if let Some(value) = pool_quota {
        sanitized.headers_mut().insert(POOL_QUOTA_HEADER, value);
    }
    if let Some(value) = retry_after {
        sanitized.headers_mut().insert(header::RETRY_AFTER, value);
    }
    sanitized
}

fn sanitize_public_model_entry(entry: Value) -> Option<Value> {
    let Value::Object(mut source) = entry else {
        return None;
    };
    let mut out = Map::new();
    for field in ["displayName", "model", "modelProvider", "tagTitle", "label"] {
        if let Some(Value::String(value)) = source.remove(field) {
            out.insert(field.into(), Value::String(value));
        }
    }
    for field in ["supportsThinking", "supportsImages", "supportsVideo", "recommended", "isInternal"] {
        if let Some(Value::Bool(value)) = source.remove(field) {
            out.insert(field.into(), Value::Bool(value));
        }
    }
    for field in ["maxTokens", "maxOutputTokens", "thinkingBudget"] {
        if let Some(Value::Number(value)) = source.remove(field)
            && value.as_u64().is_some_and(|n| n <= i32::MAX as u64)
        {
            out.insert(field.into(), Value::Number(value));
        }
    }
    if let Some(Value::Object(mimes)) = source.remove("supportedMimeTypes") {
        let mut clean_mimes = Map::new();
        for (mime, supported) in mimes {
            if let Value::Bool(flag) = supported {
                clean_mimes.insert(mime, Value::Bool(flag));
            }
        }
        if !clean_mimes.is_empty() {
            out.insert("supportedMimeTypes".into(), Value::Object(clean_mimes));
        }
    }
    if let Some(Value::Array(items)) = source.remove("supportedThinkingLevels") {
        let strings: Vec<Value> = items.into_iter().filter(|item| item.is_string()).collect();
        if !strings.is_empty() {
            out.insert("supportedThinkingLevels".into(), Value::Array(strings));
        }
    }
    Some(Value::Object(out))
}

fn filter_available_models(raw: Value, allowed: &BTreeSet<String>) -> Option<Value> {
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    let mut kept_models = Map::new();
    if let Some(Value::Object(models)) = source.remove("models") {
        for (model_id, entry) in models {
            if allowed.contains(&model_id)
                && let Some(public_entry) = sanitize_public_model_entry(entry)
            {
                kept_models.insert(model_id, public_entry);
            }
        }
    }
    out.insert("models".into(), Value::Object(kept_models.clone()));
    let default_model = source
        .remove("defaultAgentModelId")
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|id| allowed.contains(id) && kept_models.contains_key(id))
        .or_else(|| kept_models.keys().next().cloned());
    if let Some(default_model) = default_model {
        out.insert("defaultAgentModelId".into(), Value::String(default_model));
    }
    if let Some(Value::Array(sorts)) = source.remove("agentModelSorts") {
        let filtered_sorts: Vec<Value> = sorts
            .into_iter()
            .filter_map(|sort| {
                let Value::Object(mut sort_obj) = sort else {
                    return None;
                };
                let Value::Array(groups) = sort_obj.remove("groups")? else {
                    return None;
                };
                let filtered_groups: Vec<Value> = groups
                    .into_iter()
                    .filter_map(|group| {
                        let Value::Object(mut group_obj) = group else {
                            return None;
                        };
                        let Value::Array(model_ids) = group_obj.remove("modelIds")? else {
                            return None;
                        };
                        let filtered_ids: Vec<Value> = model_ids
                            .into_iter()
                            .filter(|id| id.as_str().is_some_and(|m| allowed.contains(m) && kept_models.contains_key(m)))
                            .collect();
                        if filtered_ids.is_empty() {
                            return None;
                        }
                        let mut clean_group = Map::new();
                        clean_group.insert("modelIds".into(), Value::Array(filtered_ids));
                        Some(Value::Object(clean_group))
                    })
                    .collect();
                if filtered_groups.is_empty() {
                    return None;
                }
                let mut clean_sort = Map::new();
                if let Some(Value::String(display_name)) = sort_obj.remove("displayName") {
                    clean_sort.insert("displayName".into(), Value::String(display_name));
                }
                clean_sort.insert("groups".into(), Value::Array(filtered_groups));
                Some(Value::Object(clean_sort))
            })
            .collect();
        if !filtered_sorts.is_empty() {
            out.insert("agentModelSorts".into(), Value::Array(filtered_sorts));
        }
    }
    for array_key in ["commandModelIds", "tabModelIds", "imageGenerationModelIds", "mqueryModelIds"] {
        if let Some(Value::Array(ids)) = source.remove(array_key) {
            let filtered: Vec<Value> =
                ids.into_iter().filter(|id| id.as_str().is_some_and(|m| allowed.contains(m) && kept_models.contains_key(m))).collect();
            if !filtered.is_empty() {
                out.insert(array_key.into(), Value::Array(filtered));
            }
        }
    }
    Some(Value::Object(out))
}

fn sanitize_google_part(raw: Value) -> Option<Value> {
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    if let Some(Value::String(text)) = source.remove("text") {
        out.insert("text".into(), Value::String(text));
    }
    if let Some(Value::Bool(thought)) = source.remove("thought") {
        out.insert("thought".into(), Value::Bool(thought));
    }
    if let Some(Value::String(sig)) = source.remove("thoughtSignature") {
        out.insert("thoughtSignature".into(), Value::String(sig));
    }
    if let Some(Value::Object(mut fc)) = source.remove("functionCall")
        && let Some(Value::String(name)) = fc.remove("name")
    {
        let mut clean_fc = Map::new();
        clean_fc.insert("name".into(), Value::String(name));
        if let Some(args @ Value::Object(_)) = fc.remove("args") {
            clean_fc.insert("args".into(), args);
        }
        if let Some(Value::String(id)) = fc.remove("id") {
            clean_fc.insert("id".into(), Value::String(id));
        }
        out.insert("functionCall".into(), Value::Object(clean_fc));
    }
    if let Some(Value::Object(mut fr)) = source.remove("functionResponse")
        && let Some(Value::String(name)) = fr.remove("name")
    {
        let mut clean_fr = Map::new();
        clean_fr.insert("name".into(), Value::String(name));
        if let Some(resp @ Value::Object(_)) = fr.remove("response") {
            clean_fr.insert("response".into(), resp);
        }
        if let Some(Value::String(id)) = fr.remove("id") {
            clean_fr.insert("id".into(), Value::String(id));
        }
        out.insert("functionResponse".into(), Value::Object(clean_fr));
    }
    if let Some(Value::Object(mut inline)) = source.remove("inlineData")
        && let (Some(Value::String(mime)), Some(Value::String(data))) = (inline.remove("mimeType"), inline.remove("data"))
    {
        let mut clean_inline = Map::new();
        clean_inline.insert("mimeType".into(), Value::String(mime));
        clean_inline.insert("data".into(), Value::String(data));
        out.insert("inlineData".into(), Value::Object(clean_inline));
    }
    if let Some(Value::Object(mut code)) = source.remove("executableCode")
        && let Some(Value::String(src)) = code.remove("code")
    {
        let mut clean_code = Map::new();
        if let Some(Value::String(lang)) = code.remove("language") {
            clean_code.insert("language".into(), Value::String(lang));
        }
        clean_code.insert("code".into(), Value::String(src));
        out.insert("executableCode".into(), Value::Object(clean_code));
    }
    if let Some(Value::Object(mut exec)) = source.remove("codeExecutionResult") {
        let mut clean_exec = Map::new();
        if let Some(Value::String(outcome)) = exec.remove("outcome") {
            clean_exec.insert("outcome".into(), Value::String(outcome));
        }
        if let Some(Value::String(output)) = exec.remove("output") {
            clean_exec.insert("output".into(), Value::String(output));
        }
        if !clean_exec.is_empty() {
            out.insert("codeExecutionResult".into(), Value::Object(clean_exec));
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn sanitize_google_candidate(raw: Value) -> Option<Value> {
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    if let Some(Value::Object(mut content)) = source.remove("content") {
        let mut clean_content = Map::new();
        if let Some(Value::String(role)) = content.remove("role") {
            clean_content.insert("role".into(), Value::String(role));
        }
        if let Some(Value::Array(parts)) = content.remove("parts") {
            let clean_parts: Vec<Value> = parts.into_iter().filter_map(sanitize_google_part).collect();
            clean_content.insert("parts".into(), Value::Array(clean_parts));
        }
        out.insert("content".into(), Value::Object(clean_content));
    }
    // Omit provider-authored diagnostic text such as finishMessage.
    if let Some(Value::String(reason)) = source.remove("finishReason") {
        out.insert("finishReason".into(), Value::String(reason));
    }
    if let Some(Value::Number(index)) = source.remove("index")
        && index.as_u64().is_some()
    {
        out.insert("index".into(), Value::Number(index));
    }
    Some(Value::Object(out))
}

fn sanitize_google_usage(raw: Value) -> Option<Value> {
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    for field in [
        "promptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
        "thoughtsTokenCount",
        "cachedContentTokenCount",
        "toolUsePromptTokenCount",
    ] {
        if let Some(Value::Number(value)) = source.remove(field)
            && value.as_u64().is_some()
        {
            out.insert(field.into(), Value::Number(value));
        }
    }
    Some(Value::Object(out))
}

fn sanitize_google_prompt_feedback(raw: Value) -> Option<Value> {
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    // Omit blockReasonMessage and unknown provider diagnostic fields.
    if let Some(Value::String(reason)) = source.remove("blockReason") {
        out.insert("blockReason".into(), Value::String(reason));
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn sanitize_google_response_inner(raw: Value) -> Option<Value> {
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    if let Some(Value::Array(candidates)) = source.remove("candidates") {
        let clean: Vec<Value> = candidates.into_iter().filter_map(sanitize_google_candidate).collect();
        out.insert("candidates".into(), Value::Array(clean));
    }
    if let Some(usage) = source.remove("usageMetadata").and_then(sanitize_google_usage) {
        out.insert("usageMetadata".into(), usage);
    }
    if let Some(Value::String(version)) = source.remove("modelVersion") {
        out.insert("modelVersion".into(), Value::String(version));
    }
    if let Some(feedback) = source.remove("promptFeedback").and_then(sanitize_google_prompt_feedback) {
        out.insert("promptFeedback".into(), feedback);
    }
    Some(Value::Object(out))
}

enum GoogleEnvelopeResult {
    Data(Value),
    ProviderError { sanitized: Value, status: StatusCode, raw: Value },
}

fn sanitize_google_error_value(err: &Value) -> (Value, StatusCode) {
    const RPC: [(&str, u16); 17] = [
        ("OK", 200),
        ("CANCELLED", 499),
        ("UNKNOWN", 500),
        ("INVALID_ARGUMENT", 400),
        ("DEADLINE_EXCEEDED", 504),
        ("NOT_FOUND", 404),
        ("ALREADY_EXISTS", 409),
        ("PERMISSION_DENIED", 403),
        ("RESOURCE_EXHAUSTED", 429),
        ("FAILED_PRECONDITION", 400),
        ("ABORTED", 409),
        ("OUT_OF_RANGE", 400),
        ("UNIMPLEMENTED", 501),
        ("INTERNAL", 500),
        ("UNAVAILABLE", 503),
        ("DATA_LOSS", 500),
        ("UNAUTHENTICATED", 401),
    ];
    let code_num = err.get("code").and_then(Value::as_u64);
    let rpc = err.get("status").and_then(Value::as_str);
    let http = match (code_num, rpc) {
        (Some(8 | 429), _) | (_, Some("RESOURCE_EXHAUSTED")) => Some(429),
        _ => code_num
            .and_then(|n| u16::try_from(n).ok())
            .and_then(|n| if n >= 400 { Some(n) } else { RPC.get(usize::from(n)).map(|(_, http)| *http) })
            .or_else(|| RPC.iter().find(|(name, _)| Some(*name) == rpc).map(|(_, http)| *http)),
    };
    let status = http
        .and_then(|n| StatusCode::from_u16(n).ok())
        .filter(|s| s.is_client_error() || s.is_server_error())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let code = u64::from(status.as_u16());
    let rpc_status = match status.as_u16() {
        429 => "RESOURCE_EXHAUSTED",
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        409 => "ABORTED",
        504 => "DEADLINE_EXCEEDED",
        _ => "UNAVAILABLE",
    };
    let message =
        if status == StatusCode::TOO_MANY_REQUESTS { "upstream subscription quota exhausted" } else { "upstream inference failed" };
    (
        serde_json::json!({
            "error": {
                "code": code,
                "message": message,
                "status": rpc_status,
            }
        }),
        status,
    )
}

fn extract_google_provider_error(raw: &Value) -> Option<(Value, StatusCode)> {
    let err = raw.get("error").filter(|v| !v.is_null()).or_else(|| raw.pointer("/response/error").filter(|v| !v.is_null()))?;
    Some(sanitize_google_error_value(err))
}

async fn collect_bounded_google_body(body: Body, limit: usize, outcome: Option<&crate::router::WorkerStreamGuard>) -> Result<Vec<u8>, ()> {
    let mut stream = body.into_data_stream();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + WORKER_BODY_READ_TIMEOUT;
    loop {
        let next = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                if let Some(outcome) = outcome {
                    outcome.finish_stream_error().await;
                }
                drop(stream);
                return Err(());
            }
        };
        let Some(chunk) = next else {
            break;
        };
        let Ok(bytes) = chunk else {
            if let Some(outcome) = outcome {
                outcome.finish_stream_error().await;
            }
            drop(stream);
            return Err(());
        };
        if out.len().saturating_add(bytes.len()) > limit {
            if let Some(outcome) = outcome {
                outcome.finish_stream_error().await;
            }
            drop(stream);
            return Err(());
        }
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

pub(crate) enum GoogleWorkerEnvelopeKind {
    Data,
    ProviderError,
    Invalid,
}

pub(crate) fn classify_google_worker_envelope(raw: &Value) -> GoogleWorkerEnvelopeKind {
    match sanitize_google_generation_envelope(raw.clone()) {
        Some(GoogleEnvelopeResult::Data(_)) => GoogleWorkerEnvelopeKind::Data,
        Some(GoogleEnvelopeResult::ProviderError { .. }) => GoogleWorkerEnvelopeKind::ProviderError,
        None => GoogleWorkerEnvelopeKind::Invalid,
    }
}

fn sanitize_google_generation_envelope(raw: Value) -> Option<GoogleEnvelopeResult> {
    if let Some((sanitized, status)) = extract_google_provider_error(&raw) {
        return Some(GoogleEnvelopeResult::ProviderError { sanitized, status, raw });
    }
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    if let Some(response) = source.remove("response") {
        out.insert("response".into(), sanitize_google_response_inner(response)?);
    } else if source.contains_key("candidates") || source.contains_key("usageMetadata") || source.contains_key("promptFeedback") {
        return Some(GoogleEnvelopeResult::Data(sanitize_google_response_inner(Value::Object(source))?));
    }
    Some(GoogleEnvelopeResult::Data(Value::Object(out)))
}

fn sanitize_google_json_payload(raw: Value) -> Option<(Value, Option<(StatusCode, Value)>)> {
    match raw {
        Value::Array(items) => {
            let mut clean = Vec::with_capacity(items.len());
            let mut provider_error = None;
            for item in items {
                match sanitize_google_generation_envelope(item)? {
                    GoogleEnvelopeResult::Data(value) => clean.push(value),
                    GoogleEnvelopeResult::ProviderError { sanitized, status, raw } => {
                        if provider_error.is_none() {
                            provider_error = Some((status, raw));
                        }
                        clean.push(sanitized);
                    }
                }
            }
            Some((Value::Array(clean), provider_error))
        }
        other => match sanitize_google_generation_envelope(other)? {
            GoogleEnvelopeResult::Data(value) => Some((value, None)),
            GoogleEnvelopeResult::ProviderError { sanitized, status, raw } => Some((sanitized, Some((status, raw)))),
        },
    }
}

fn sanitize_google_count_tokens_response(raw: Value) -> Option<Value> {
    let Value::Object(mut source) = raw else {
        return None;
    };
    let mut out = Map::new();
    for field in ["totalTokens", "cachedContentTokenCount"] {
        if let Some(Value::Number(value)) = source.remove(field)
            && value.as_u64().is_some()
        {
            out.insert(field.into(), Value::Number(value));
        }
    }
    Some(Value::Object(out))
}

enum SseFrameResult {
    Emit(Bytes),
    ProviderError { frame: Bytes, status: StatusCode, raw: Value },
    Skip,
    Invalid,
}

fn sanitize_google_sse_event(data_lines: &[String]) -> SseFrameResult {
    if data_lines.is_empty() {
        return SseFrameResult::Skip;
    }
    let payload = data_lines.join("\n");
    let Ok(parsed) = serde_json::from_str::<Value>(&payload) else {
        return SseFrameResult::Invalid;
    };
    match sanitize_google_generation_envelope(parsed) {
        Some(GoogleEnvelopeResult::Data(clean)) => match serde_json::to_string(&clean) {
            Ok(encoded) => SseFrameResult::Emit(Bytes::from(format!("data: {encoded}\n\n"))),
            Err(_) => SseFrameResult::Invalid,
        },
        Some(GoogleEnvelopeResult::ProviderError { sanitized, status, raw }) => match serde_json::to_string(&sanitized) {
            Ok(encoded) => SseFrameResult::ProviderError { frame: Bytes::from(format!("data: {encoded}\n\n")), status, raw },
            Err(_) => SseFrameResult::Invalid,
        },
        None => SseFrameResult::Invalid,
    }
}

fn take_sse_line(buf: &mut Vec<u8>, skip_lf: &mut bool, eof: bool) -> Option<Vec<u8>> {
    if *skip_lf {
        if buf.starts_with(b"\n") {
            buf.drain(..1);
            *skip_lf = false;
        } else if !buf.is_empty() || eof {
            *skip_lf = false;
        }
    }
    for i in 0..buf.len() {
        if buf[i] == b'\r' {
            let line = buf[..i].to_vec();
            if i + 1 < buf.len() && buf[i + 1] == b'\n' {
                buf.drain(..i + 2);
            } else {
                buf.drain(..=i);
                *skip_lf = true;
            }
            return Some(line);
        }
        if buf[i] == b'\n' {
            let line = buf[..i].to_vec();
            buf.drain(..=i);
            return Some(line);
        }
    }
    if eof && !buf.is_empty() {
        return Some(std::mem::take(buf));
    }
    None
}

fn sse_invalid_stream_frame() -> Bytes {
    Bytes::from_static(b"data: {\"error\":{\"code\":502,\"message\":\"upstream stream invalid\",\"status\":\"UNAVAILABLE\"}}\n\n")
}

fn build_google_sse_response(response: Response) -> Response {
    let status = response.status();
    let stream_outcome = response.extensions().get::<WorkerStreamOutcome>().cloned().map(WorkerStreamOutcome::into_guard);
    let upstream = response.into_body().into_data_stream();
    let sanitized = stream::unfold(
        (upstream, Vec::<u8>::new(), Vec::<String>::new(), 0usize, true, false, false, stream_outcome),
        |(mut upstream, mut buf, mut data_lines, mut event_bytes, mut check_bom, mut skip_lf, mut done, outcome)| async move {
            loop {
                if check_bom && (buf.len() >= 3 || done) {
                    if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
                        buf.drain(..3);
                    }
                    check_bom = false;
                }
                if !check_bom && let Some(raw_line) = take_sse_line(&mut buf, &mut skip_lf, done) {
                    if raw_line.is_empty() {
                        let lines = std::mem::take(&mut data_lines);
                        event_bytes = 0;
                        match sanitize_google_sse_event(&lines) {
                            SseFrameResult::Emit(clean) => {
                                return Some((
                                    Ok::<Bytes, std::io::Error>(clean),
                                    (upstream, buf, data_lines, event_bytes, false, skip_lf, done, outcome),
                                ));
                            }
                            SseFrameResult::ProviderError { frame, status, raw } => {
                                if let Some(outcome) = outcome.as_ref() {
                                    outcome.dispatch_provider_error(status, raw).await;
                                }
                                return Some((
                                    Ok::<Bytes, std::io::Error>(frame),
                                    (upstream, buf, data_lines, event_bytes, false, skip_lf, done, outcome),
                                ));
                            }
                            SseFrameResult::Skip => continue,
                            SseFrameResult::Invalid => {
                                if let Some(outcome) = outcome.as_ref() {
                                    outcome.finish_stream_error().await;
                                }
                                return Some((
                                    Ok::<Bytes, std::io::Error>(sse_invalid_stream_frame()),
                                    (upstream, Vec::new(), Vec::new(), 0, false, false, true, outcome),
                                ));
                            }
                        }
                    }
                    let Ok(line) = std::str::from_utf8(&raw_line) else {
                        if let Some(outcome) = outcome.as_ref() {
                            outcome.finish_stream_error().await;
                        }
                        return Some((
                            Ok::<Bytes, std::io::Error>(sse_invalid_stream_frame()),
                            (upstream, Vec::new(), Vec::new(), 0, false, false, true, outcome),
                        ));
                    };
                    if line.starts_with(':') {
                        continue;
                    }
                    // Per WHATWG SSE, a line with no colon is a field with an empty string value.
                    let (field, value) = match line.split_once(':') {
                        Some((field, rest)) => (field, rest.strip_prefix(' ').unwrap_or(rest)),
                        None => (line, ""),
                    };
                    if field == "data" {
                        event_bytes = event_bytes.saturating_add(value.len()).saturating_add(1);
                        if event_bytes > GOOGLE_SSE_FRAME_LIMIT {
                            if let Some(outcome) = outcome.as_ref() {
                                outcome.finish_stream_error().await;
                            }
                            return Some((
                                Ok::<Bytes, std::io::Error>(sse_invalid_stream_frame()),
                                (upstream, Vec::new(), Vec::new(), 0, false, false, true, outcome),
                            ));
                        }
                        data_lines.push(value.to_owned());
                    }
                    continue;
                }
                if done {
                    if !data_lines.is_empty() {
                        // Per WHATWG SSE, events dispatch only on a blank-line boundary;
                        // uncommitted data at EOF indicates a truncated stream.
                        data_lines.clear();
                        if let Some(outcome) = outcome.as_ref() {
                            outcome.finish_stream_error().await;
                        }
                        return Some((
                            Ok::<Bytes, std::io::Error>(sse_invalid_stream_frame()),
                            (upstream, Vec::new(), Vec::new(), 0, false, false, true, outcome),
                        ));
                    }
                    if let Some(outcome) = outcome.as_ref() {
                        outcome.finish_ok().await;
                    }
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => {
                        if buf.len().saturating_add(chunk.len()) > GOOGLE_SSE_FRAME_LIMIT {
                            if let Some(outcome) = outcome.as_ref() {
                                outcome.finish_stream_error().await;
                            }
                            return Some((
                                Ok::<Bytes, std::io::Error>(sse_invalid_stream_frame()),
                                (upstream, Vec::new(), Vec::new(), 0, false, false, true, outcome),
                            ));
                        }
                        buf.extend_from_slice(&chunk);
                    }
                    Some(Err(_)) => {
                        if let Some(outcome) = outcome.as_ref() {
                            outcome.finish_stream_error().await;
                        }
                        return Some((
                            Ok::<Bytes, std::io::Error>(sse_invalid_stream_frame()),
                            (upstream, Vec::new(), Vec::new(), 0, false, false, true, outcome),
                        ));
                    }
                    None => done = true,
                }
            }
        },
    );
    let mut out = Response::new(Body::from_stream(sanitized));
    *out.status_mut() = status;
    out.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("text/event-stream"));
    out.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-cache"));
    out
}

async fn google_fetch_available_models(State((proxy, scope)): State<(Arc<Proxy>, Arc<GoogleScope>)>, request: Request) -> Response {
    let (parts, object, _, _permit) = match prepare_google_request(&proxy, &scope, request, false, &["project"]).await {
        Ok(prepared) => prepared,
        Err(status) => return status.into_response(),
    };
    if object.get("project").is_some_and(|value| !value.is_string()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    drop(object);
    // Always discard caller-supplied project and inject sentinel rewritten per resolved OAuth account.
    let mut outbound = Map::new();
    outbound.insert("project".into(), Value::String("worker-project".into()));
    let Ok(body) = serde_json::to_vec(&outbound) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(endpoint) = scope.upstream_base.join(GOOGLE_FETCH_AVAILABLE_MODELS) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let response = forward_authorized(proxy, parts, body.into(), Value::Object(outbound), Lane::Google, Some(&endpoint)).await;
    if !response.status().is_success() {
        return sanitize_google_worker_error(response);
    }
    let outcome = response.extensions().get::<WorkerStreamOutcome>().cloned().map(WorkerStreamOutcome::into_guard);
    let Ok(bytes) = collect_bounded_google_body(response.into_body(), GOOGLE_CATALOG_RESPONSE_LIMIT, outcome.as_ref()).await else {
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream model catalog unreadable");
    };
    let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) else {
        if let Some(outcome) = outcome.as_ref() {
            outcome.finish_stream_error().await;
        }
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream model catalog invalid");
    };
    if let Some((sanitized, status)) = extract_google_provider_error(&parsed) {
        if let Some(outcome) = outcome.as_ref() {
            outcome.dispatch_provider_error(status, parsed).await;
            outcome.finish_ok().await;
        }
        return (status, axum::Json(sanitized)).into_response();
    }
    let Some(filtered) = filter_available_models(parsed, &scope.models) else {
        if let Some(outcome) = outcome.as_ref() {
            outcome.finish_stream_error().await;
        }
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream model catalog invalid");
    };
    if let Some(outcome) = outcome.as_ref() {
        outcome.finish_ok().await;
    }
    axum::Json(filtered).into_response()
}

async fn google_count_tokens(State((proxy, scope)): State<(Arc<Proxy>, Arc<GoogleScope>)>, request: Request) -> Response {
    let (parts, mut object, _, _permit) = match prepare_google_request(&proxy, &scope, request, false, &GOOGLE_COUNT_FIELDS).await {
        Ok(prepared) => prepared,
        Err(status) => return status.into_response(),
    };
    if object.get("project").is_some_and(|value| !value.is_string()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let top_model = match object.remove("model") {
        None => None,
        Some(Value::String(model)) => Some(model.strip_prefix("models/").unwrap_or(model.as_str()).to_owned()),
        Some(_) => return StatusCode::FORBIDDEN.into_response(),
    };
    let Some(Value::Object(mut req_obj)) = object.remove("request") else {
        return StatusCode::FORBIDDEN.into_response();
    };
    drop(object);
    if req_obj.keys().any(|key| !GOOGLE_COUNT_REQUEST_FIELDS.contains(&key.as_str())) || map_has_forbidden_google_key(&req_obj) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let nested_model = match req_obj.remove("model") {
        None => None,
        Some(Value::String(model)) => Some(model.strip_prefix("models/").unwrap_or(model.as_str()).to_owned()),
        Some(_) => return StatusCode::FORBIDDEN.into_response(),
    };
    let resolved_model = match (top_model, nested_model) {
        (Some(top), Some(nested)) if top == nested => top,
        (Some(top), None) => top,
        (None, Some(nested)) => nested,
        _ => return StatusCode::FORBIDDEN.into_response(),
    };
    if !scope.models.contains(&resolved_model) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Always write the validated canonical model into `request.model` and omit caller project.
    let mut clean_req = Map::new();
    clean_req.insert("model".into(), Value::String(format!("models/{resolved_model}")));
    for field in GOOGLE_COUNT_REQUEST_FIELDS {
        if field != "model"
            && let Some(value) = req_obj.remove(field)
        {
            clean_req.insert(field.into(), value);
        }
    }
    drop(req_obj);
    let mut outbound = Map::new();
    outbound.insert("request".into(), Value::Object(clean_req));
    let Ok(body) = serde_json::to_vec(&outbound) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(endpoint) = scope.upstream_base.join(GOOGLE_COUNT_TOKENS) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let response = forward_authorized(proxy, parts, body.into(), Value::Object(outbound), Lane::Google, Some(&endpoint)).await;
    if !response.status().is_success() {
        return sanitize_google_worker_error(response);
    }
    let outcome = response.extensions().get::<WorkerStreamOutcome>().cloned().map(WorkerStreamOutcome::into_guard);
    let Ok(bytes) = collect_bounded_google_body(response.into_body(), GOOGLE_JSON_RESPONSE_LIMIT, outcome.as_ref()).await else {
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream response unreadable");
    };
    let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) else {
        if let Some(outcome) = outcome.as_ref() {
            outcome.finish_stream_error().await;
        }
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream response invalid");
    };
    if let Some((sanitized, status)) = extract_google_provider_error(&parsed) {
        if let Some(outcome) = outcome.as_ref() {
            outcome.dispatch_provider_error(status, parsed).await;
            outcome.finish_ok().await;
        }
        return (status, axum::Json(sanitized)).into_response();
    }
    let Some(clean) = sanitize_google_count_tokens_response(parsed) else {
        if let Some(outcome) = outcome.as_ref() {
            outcome.finish_stream_error().await;
        }
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream response invalid");
    };
    if let Some(outcome) = outcome.as_ref() {
        outcome.finish_ok().await;
    }
    axum::Json(clean).into_response()
}

async fn google_forward_generate(
    proxy: Arc<Proxy>,
    scope: Arc<GoogleScope>,
    request: Request,
    path: &'static str,
    allow_sse_query: bool,
) -> Response {
    let (parts, mut object, query, _permit) =
        match prepare_google_request(&proxy, &scope, request, allow_sse_query, &GOOGLE_GENERATE_FIELDS).await {
            Ok(prepared) => prepared,
            Err(status) => return status.into_response(),
        };
    if object.get("project").is_some_and(|value| !value.is_string()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(Value::String(model)) = object.remove("model") else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if !scope.models.contains(&model) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(Value::Object(mut req_obj)) = object.remove("request") else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if req_obj.keys().any(|key| !GOOGLE_GENERATE_REQUEST_FIELDS.contains(&key.as_str())) || map_has_forbidden_google_key(&req_obj) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut clean_req = Map::new();
    for field in GOOGLE_GENERATE_REQUEST_FIELDS {
        if let Some(value) = req_obj.remove(field) {
            clean_req.insert(field.into(), value);
        }
    }
    drop(req_obj);
    // Discard caller-supplied project and inject sentinel rewritten from the resolved OAuth account.
    let mut outbound = Map::new();
    outbound.insert("model".into(), Value::String(model));
    outbound.insert("project".into(), Value::String("worker-project".into()));
    outbound.insert("request".into(), Value::Object(clean_req));
    for field in ["requestId", "requestType", "userAgent"] {
        match object.remove(field) {
            None => {}
            Some(Value::String(text)) => {
                outbound.insert(field.into(), Value::String(text));
            }
            Some(_) => return StatusCode::FORBIDDEN.into_response(),
        }
    }
    drop(object);
    let Ok(body) = serde_json::to_vec(&outbound) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(mut endpoint) = scope.upstream_base.join(path) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if let Some(query) = query {
        endpoint.set_query(Some(&query));
    }
    let response = forward_authorized(proxy, parts, body.into(), Value::Object(outbound), Lane::Google, Some(&endpoint)).await;
    if !response.status().is_success() {
        return sanitize_google_worker_error(response);
    }
    if crate::router::is_sse_media_type(response.headers()) {
        return build_google_sse_response(response);
    }
    let outcome = response.extensions().get::<WorkerStreamOutcome>().cloned().map(WorkerStreamOutcome::into_guard);
    let Ok(bytes) = collect_bounded_google_body(response.into_body(), GOOGLE_JSON_RESPONSE_LIMIT, outcome.as_ref()).await else {
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream response unreadable");
    };
    let Ok(parsed) = serde_json::from_slice::<Value>(&bytes) else {
        if let Some(outcome) = outcome.as_ref() {
            outcome.finish_stream_error().await;
        }
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream response invalid");
    };
    let Some((clean, provider_error)) = sanitize_google_json_payload(parsed) else {
        if let Some(outcome) = outcome.as_ref() {
            outcome.finish_stream_error().await;
        }
        return lane_error(Lane::Google, StatusCode::BAD_GATEWAY, "upstream response invalid");
    };
    let standalone_error_status = (!clean.is_array()).then_some(()).and(provider_error.as_ref().map(|(status, _)| *status));
    if let Some(outcome) = outcome.as_ref() {
        if let Some((status, raw)) = provider_error {
            outcome.dispatch_provider_error(status, raw).await;
        }
        outcome.finish_ok().await;
    }
    if let Some(status) = standalone_error_status {
        return (status, axum::Json(clean)).into_response();
    }
    axum::Json(clean).into_response()
}

async fn google_generate_content(State((proxy, scope)): State<(Arc<Proxy>, Arc<GoogleScope>)>, request: Request) -> Response {
    google_forward_generate(proxy, scope, request, GOOGLE_GENERATE_CONTENT, false).await
}

async fn google_stream_generate_content(State((proxy, scope)): State<(Arc<Proxy>, Arc<GoogleScope>)>, request: Request) -> Response {
    google_forward_generate(proxy, scope, request, GOOGLE_STREAM_GENERATE_CONTENT, true).await
}
