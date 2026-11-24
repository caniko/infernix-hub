//! A separate, deny-by-default HTTP surface for Codex subscription workers.
use std::{
    collections::BTreeSet,
    fs,
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
    body::to_bytes,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use clap::Args;
use reqwest::Url;
use serde_json::Value;
use tokio::net::{TcpListener, UnixListener, UnixStream};

use crate::{
    Proxy,
    config::{Config, matches_secret, private_dir, secret_file, validate_secret},
    pool::{Lane, bounded_identifier},
    router::{REQUEST_BODY_LIMIT, bearer, forward_authorized, scan_unique_fields},
};

pub(crate) const CODEX_RESPONSES: &str = "/backend-api/codex/responses";

#[derive(Args, Default)]
pub struct WorkerOptions {
    /// Bind a worker socket in a private directory owned by the service user.
    #[arg(long, conflicts_with = "worker_listen")]
    pub worker_unix_socket: Option<PathBuf>,
    /// Bind the worker listener to a numeric loopback address.
    #[arg(long)]
    pub worker_listen: Option<SocketAddr>,
    /// Dedicated worker bearer; prefer the file option to keep it out of process arguments.
    #[arg(long, conflicts_with = "worker_api_key_file")]
    pub worker_api_key: Option<String>,
    /// File containing the dedicated worker bearer (required on TCP unless passed directly).
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
}

impl WorkerOptions {
    pub fn validate(self, shared: &Config) -> Result<Option<WorkerConfig>> {
        let bind = match (self.worker_unix_socket, self.worker_listen) {
            (None, None) => {
                ensure!(
                    self.worker_api_key.is_none() && self.worker_api_key_file.is_none() && self.worker_allowed_models.is_empty(),
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
        let key = match (self.worker_api_key, self.worker_api_key_file) {
            (Some(key), None) => Some(validate_secret(&key)?),
            (None, Some(path)) => Some(secret_file(&path)?),
            (None, None) => None,
            (Some(_), Some(_)) => bail!("configure only one source for the worker API key"),
        };
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
            },
        }))
    }
}

impl WorkerConfig {
    pub async fn bind(self) -> Result<BoundWorker> {
        let listener = match self.bind {
            Bind::Tcp(addr) => Listener::Tcp(TcpListener::bind(addr).await.context("bind worker TCP listener")?),
            Bind::Unix(path) => {
                Listener::Unix(bind_unix(&path).await.with_context(|| format!("bind worker Unix socket {}", path.display()))?)
            }
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
    Unix(UnixListener),
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
            Listener::Unix(listener) => {
                tracing::info!(addr = ?listener.local_addr()?, "scoped worker proxy listening");
                axum::serve(listener, app).with_graceful_shutdown(shutdown).await
            }
        }
    }
}

async fn bind_unix(path: &Path) -> Result<UnixListener> {
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
        ensure!(
            metadata.is_dir() && (metadata.uid() == 0 || metadata.uid() == uid),
            "worker socket ancestry must consist of trusted directories (no symlinks)"
        );
        ensure!(mode & 0o022 == 0 || mode & 0o1000 != 0, "worker socket ancestry is writable by other users");
        if dir == parent {
            ensure!(metadata.uid() == uid && mode & 0o777 == 0o700, "worker socket parent must be owned by the service user and mode 0700");
        }
    }
    // Serialize stale-socket checks with other starts in this private directory.
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
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
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
            | "connection"
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

async fn forward(State((proxy, scope)): State<(Arc<Proxy>, Arc<Scope>)>, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    if let Some(key) = &scope.key
        && (parts.headers.get_all(header::AUTHORIZATION).iter().count() != 1
            || !bearer(&parts.headers).is_some_and(|token| matches_secret(token, key)))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if parts.uri.query().is_some()
        || parts.uri.authority().is_some()
        || parts.headers.keys().any(|name| !local_header(name.as_str()) && !inference_header(name.as_str()))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(body) = to_bytes(body, REQUEST_BODY_LIMIT).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let Ok(raw) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(object) = raw.as_object() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !object.get("model").and_then(Value::as_str).is_some_and(|model| scope.models.contains(model))
        || object.keys().any(|key| !BODY_FIELDS.contains(&key.as_str()))
        || scan_unique_fields(&body, BODY_FIELDS, |_, _| Some(())).is_none()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Do not pass inbound credentials, transport headers, or client telemetry
    // to the shared forwarding path.
    parts.headers = parts
        .headers
        .iter()
        .filter(|(name, _)| inference_header(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    parts.headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    forward_authorized(proxy, parts, body, raw, Lane::Codex, Some(&scope.upstream)).await
}
