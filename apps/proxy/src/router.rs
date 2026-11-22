use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, TimeDelta, Utc};
use contracts::providers::{ActivityAttemptIn, ActivityExchangeIn, ActivityPurpose, ActivitySource, CaptureState, TraceEdgeKind};
use futures_util::stream;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    ACTIVITY_CAP, CAPTURE_BYTES, Proxy,
    config::matches_secret,
    openai_account,
    pool::{Account, Lane, text},
};

pub(crate) fn routes() -> Router<Arc<Proxy>> {
    Router::new()
        .route("/v1/messages", post(forward))
        .route("/v1/messages/count_tokens", post(forward))
        .route("/v1/responses", post(forward))
        .route("/v1/chat/completions", post(forward))
        .route("/v1/models", get(forward))
        .route("/api/hello", get(|| async { axum::Json(json!({"message":"hello"})) }))
        .route("/backend-api/codex/{*rest}", any(forward))
        .route("/v1internal/{*rest}", any(forward))
        .route("/v1beta/{*rest}", any(forward))
        .route("/{action}", any(forward))
}

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, secret) = headers.get(header::AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(secret).filter(|s| !s.is_empty())
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, axum::Json(json!({"error":{"message":message}}))).into_response()
}

async fn forward(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    let native = path.starts_with("/backend-api/codex/");
    let lane = match path {
        "/v1/messages" | "/v1/messages/count_tokens" => Lane::Claude,
        "/v1/models" | "/v1/responses" | "/v1/chat/completions" => Lane::Codex,
        _ if native => Lane::Codex,
        _ if path.starts_with("/v1internal/") || path.starts_with("/v1internal:") || path.starts_with("/v1beta/") => Lane::Google,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let provided = bearer(&parts.headers);
    let authorized = [parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()), provided]
        .into_iter()
        .flatten()
        .any(|key| matches_secret(key, &proxy.config.client_key));
    if !authorized
        && !(lane == Lane::Codex
            && match provided {
                Some(token) => proxy.authorize_codex(token).await,
                None => false,
            })
        && !(lane == Lane::Google
            && match provided {
                Some(token) => proxy.authorize_google(token).await,
                None => false,
            })
    {
        return error(StatusCode::UNAUTHORIZED, "invalid inference credentials");
    }
    if parts.method == axum::http::Method::CONNECT || parts.method == axum::http::Method::TRACE {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Ok(body) = to_bytes(body, 32 * 1024 * 1024).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
    };
    let raw: Value = serde_json::from_slice(&body).unwrap_or_default();
    let lane = if lane == Lane::Codex && path == "/v1/chat/completions" && text(&raw, &["model"]).is_some_and(proxy_model_is_go) {
        Lane::Go
    } else {
        lane
    };
    let model = text(&raw, &["model"]).filter(|s| identifier(s)).unwrap_or_default();
    let metadata_user: Value = text(&raw, &["/metadata/user_id"]).and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    let session = header_id(&parts.headers, &["x-session-id", "x-claude-session-id", "session_id"])
        .or_else(|| {
            text(
                &raw,
                &[
                    "/metadata/session_id",
                    "/metadata/sessionId",
                    "/request/session_id",
                    "/request/sessionId",
                    "session_id",
                    "trajectoryId",
                    "prompt_cache_key",
                ],
            )
        })
        .or_else(|| text(&metadata_user, &["session_id"]))
        .or_else(|| text(&raw, &["/metadata/user_id"]).and_then(|s| s.rsplit_once("_session_").map(|(_, id)| id)))
        .filter(|s| identifier(s))
        .map(str::to_owned);
    let parent_header = header_id(&parts.headers, &["x-parent-session-id", "x-claude-parent-session-id"]);
    let parent = parent_header
        .or_else(|| text(&raw, &["/metadata/parent_session_id", "/metadata/parentSessionId"]))
        .or_else(|| text(&metadata_user, &["parent_session_id"]))
        .filter(|s| identifier(s) && Some(*s) != session.as_deref())
        .map(str::to_owned);
    let purpose = match parts.headers.get("x-cpa-purpose").and_then(|v| v.to_str().ok()) {
        Some("window-start") => ActivityPurpose::WindowStarter,
        Some("canary") => ActivityPurpose::Canary,
        Some("thinking-test") => ActivityPurpose::ThinkingTest,
        Some("eval-replay") => ActivityPurpose::EvalReplay,
        _ => ActivityPurpose::Ordinary,
    };
    let (policy, moved_from, capture) = {
        let state = proxy.state.read().await;
        (
            state.pool.policy.clone(),
            session.as_deref().and_then(|s| state.pool.bound(lane, s)).map(str::to_owned),
            state.captures.enabled().then_some((state.captures.epoch, state.captures.lease_generation)),
        )
    };
    let mut exchange = Exchange::new(proxy.clone(), lane, model, session, parent, purpose);
    if let Some((epoch, lease_generation)) = capture {
        exchange.capture = Some(Capture {
            epoch,
            lease_generation,
            method: parts.method.to_string(),
            path: capture_path(path).into(),
            request: if body.len() <= CAPTURE_BYTES { transformed(&raw) } else { None },
            request_size: body.len(),
            response_type: String::new(),
        });
    }
    let mut tried = Vec::new();
    let mut last_error = StatusCode::SERVICE_UNAVAILABLE;
    loop {
        let (account, reset) = {
            let mut state = proxy.state.write().await;
            let pool = &mut state.pool;
            let account = pool.pick(lane, exchange.session.as_deref(), exchange.parent.as_deref(), model, &tried);
            let reset = if account.is_none() && tried.is_empty() { pool.next_reset(lane, model) } else { None };
            (account, reset)
        };
        let Some(account) = account else {
            if let Some(reset) = reset {
                exchange.finish(Some(StatusCode::TOO_MANY_REQUESTS), "rate_limited", Usage::default(), None, None).await;
                return pool_limit(lane, reset);
            }
            exchange.finish(Some(last_error), "unavailable", Usage::default(), None, None).await;
            return error(last_error, "no eligible upstream account");
        };
        tried.push(account.id.clone());
        let began = Instant::now();
        let started = Utc::now();
        let timeout = Duration::from_secs(policy.first_byte_timeout_secs.clamp(1, 600));
        let incoming = async {
            let url = upstream(&proxy, &account, &parts.uri).ok_or(())?;
            let payload = request_body(&body, &raw, lane, &account);
            let build = |token: String| {
                let mut req = proxy.http.request(parts.method.clone(), url.clone()).body(payload.clone());
                let connection = connection_headers(&parts.headers);
                for (key, value) in &parts.headers {
                    let name = key.as_str();
                    if (request_header(name) || (name == "anthropic-beta" && account.api_key && lane == Lane::Claude))
                        && !connection.iter().any(|s| s.eq_ignore_ascii_case(name))
                    {
                        req = req.header(key, value);
                    }
                }
                match lane {
                    Lane::Claude if account.api_key => req.header("x-api-key", token),
                    Lane::Claude => {
                        let beta = parts.headers.get("anthropic-beta").and_then(|v| v.to_str().ok()).unwrap_or_default();
                        let beta = if beta.split(',').any(|item| item.trim() == "oauth-2025-04-20") {
                            beta.to_owned()
                        } else if beta.is_empty() {
                            "oauth-2025-04-20".into()
                        } else {
                            format!("{beta},oauth-2025-04-20")
                        };
                        req.header("anthropic-beta", beta).bearer_auth(token)
                    }
                    Lane::Codex => openai_account(req.bearer_auth(token), &account),
                    Lane::Google if account.api_key => req.header("x-goog-api-key", token),
                    _ => req.bearer_auth(token),
                }
            };
            let token = proxy.token(&account, false).await.map_err(|_| ())?;
            let mut res = build(token).send().await.map_err(|_| ())?;
            if res.status() == StatusCode::UNAUTHORIZED
                && lane == Lane::Google
                && !account.api_key
                && account.refresh.is_some()
                && let Ok(fresh) = proxy.token(&account, true).await
            {
                res = build(fresh).send().await.map_err(|_| ())?;
            }
            let first = res.chunk().await.map_err(|_| ())?;
            Ok::<_, ()>((res, first))
        };
        let (response, first) = match tokio::time::timeout(timeout, incoming).await {
            Ok(Ok(result)) => result,
            failure => {
                let timed_out = failure.is_err();
                last_error = if timed_out { StatusCode::GATEWAY_TIMEOUT } else { StatusCode::BAD_GATEWAY };
                exchange.attempt(
                    &account,
                    moved_from.as_deref(),
                    started,
                    began,
                    None,
                    if timed_out { "first_byte_timeout" } else { "upstream_error" },
                );
                result(&proxy, &account, None, false).await;
                exchange.complete_attempt(Usage::default(), None);
                continue;
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        let cooldown = (status == StatusCode::TOO_MANY_REQUESTS).then(|| cooldown(&headers, policy.failover_cooldown_secs));
        let failover = cooldown.is_some() || (status.is_server_error() && policy.switch_on_server_error);
        exchange.attempt(
            &account,
            moved_from.as_deref(),
            started,
            began,
            Some(status),
            if status.is_success() { "success" } else { "http_error" },
        );
        if !status.is_success() {
            result(&proxy, &account, cooldown, false).await;
        }
        if failover {
            let mut state = proxy.state.write().await;
            // Preview on the real scheduler; retries do not increment the round-robin cursor.
            let next = state.pool.pick(lane, exchange.session.as_deref(), exchange.parent.as_deref(), model, &tried);
            if next.is_some() {
                exchange.complete_attempt(Usage::default(), Some(millis(began.elapsed())));
                drop(state);
                proxy.log("warn", "upstream request failed over").await;
                continue;
            }
        }
        if status.is_success() {
            let mut state = proxy.state.write().await;
            if let Some(session) = &exchange.session {
                let current = state.pool.bound(lane, session);
                // Do not overwrite a conversation moved while this request was in flight.
                if current.is_none() || current == moved_from.as_deref() || current == Some(&account.id) {
                    state.pool.bind(lane, session, &account.id);
                    if let Some(parent) = &exchange.parent {
                        state.pool.link(lane, session, parent);
                    }
                }
            }
        }
        let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let sse = content_type.starts_with("text/event-stream");
        if let Some(capture) = &mut exchange.capture {
            capture.response_type = if sse { "text/event-stream" } else { "application/json" }.into();
        }
        let guard = RecordOnDrop(Some(exchange));
        let state = Wire {
            proxy: proxy.clone(),
            guard,
            account,
            status,
            started: began,
            upstream: response,
            first,
            sse,
            usage: Usage::default(),
            usage_buf: Vec::new(),
            discard_line: false,
            capture_buf: Vec::new(),
            response_size: 0,
        };
        let mut response = Response::new(Body::from_stream(stream::try_unfold(state, Wire::next)));
        *response.status_mut() = status;
        let connection = connection_headers(&headers);
        for (key, value) in &headers {
            let name = key.as_str();
            if (matches!(
                name,
                "content-type" | "content-encoding" | "cache-control" | "retry-after" | "request-id" | "x-request-id" | "x-should-retry"
            ) || name.starts_with("anthropic-ratelimit-")
                || name.starts_with("x-codex-")
                || name.starts_with("x-ratelimit-"))
                && !connection.iter().any(|s| s.eq_ignore_ascii_case(name))
            {
                response.headers_mut().append(key, value.clone());
            }
        }
        return response;
    }
}

fn pool_limit(lane: Lane, reset: DateTime<Utc>) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(json!({
            "type":"error","error":{"type":"usage_limit_reached","message":"all eligible accounts are rate limited"}
        })),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert("x-should-retry", header::HeaderValue::from_static("false"));
    let set = |headers: &mut HeaderMap, name: &'static str, n: i64| {
        if let Ok(value) = header::HeaderValue::from_str(&n.to_string()) {
            headers.insert(name, value);
        }
    };
    if reset != DateTime::<Utc>::MAX_UTC {
        set(headers, "retry-after", (reset - Utc::now()).num_seconds().max(1));
        if lane == Lane::Claude {
            headers.insert("anthropic-ratelimit-unified-status", header::HeaderValue::from_static("rejected"));
            set(headers, "anthropic-ratelimit-unified-reset", reset.timestamp());
        }
        if lane == Lane::Codex {
            set(headers, "x-codex-primary-used-percent", 100);
            set(headers, "x-codex-primary-reset-at", reset.timestamp());
        }
    }
    response
}

pub(crate) async fn run_window_start(
    proxy: Arc<Proxy>,
    account: &Account,
    model: &str,
    request: reqwest::RequestBuilder,
) -> Result<(), ()> {
    let mut exchange = Exchange::new(proxy.clone(), account.lane, model, None, None, ActivityPurpose::WindowStarter);
    let began = Instant::now();
    let started = Utc::now();
    let response = request.timeout(Duration::from_secs(45)).send().await;
    let mut usage = Usage::default();
    let (status, outcome, limited) = if let Ok(response) = response {
        let status = response.status();
        let limited = if status == StatusCode::TOO_MANY_REQUESTS {
            Some(cooldown(response.headers(), proxy.state.read().await.pool.policy.failover_cooldown_secs))
        } else {
            None
        };
        let sse =
            response.headers().get(header::CONTENT_TYPE).and_then(|h| h.to_str().ok()).is_some_and(|h| h.starts_with("text/event-stream"));
        let outcome = if !status.is_success() {
            "http_error"
        } else if let Ok(body) = crate::bounded_body(response, 256 * 1024).await {
            if sse {
                for line in body.split(|b| *b == b'\n') {
                    observe_line(&mut usage, line, account.lane);
                }
            } else if let Ok(raw) = serde_json::from_slice(&body) {
                usage.observe(&raw, account.lane);
            }
            "success"
        } else {
            "stream_error"
        };
        (Some(status), outcome, limited)
    } else {
        (None, "upstream_error", None)
    };
    exchange.attempt(account, None, started, began, status, outcome);
    let success = outcome == "success";
    result(&proxy, account, limited, success).await;
    exchange.finish(status, outcome, usage, Some(millis(began.elapsed())), None).await;
    if success { Ok(()) } else { Err(()) }
}

fn request_body(body: &Bytes, raw: &Value, lane: Lane, account: &Account) -> Bytes {
    if lane == Lane::Google
        && let Some(project) = account.project.as_deref()
        && let Some(obj) = raw.as_object()
        && obj.get("project").and_then(Value::as_str).is_some_and(|current| current != project)
    {
        let mut rewritten = obj.clone();
        rewritten.insert("project".into(), Value::String(project.to_owned()));
        if let Ok(bytes) = serde_json::to_vec(&rewritten) {
            return Bytes::from(bytes);
        }
    }
    body.clone()
}

fn proxy_model_is_go(model: &str) -> bool {
    model.starts_with("opencode-go/")
}

fn capture_path(path: &str) -> &str {
    match path {
        "/v1/messages" | "/v1/messages/count_tokens" | "/v1/responses" | "/v1/chat/completions" | "/v1/models" => path,
        _ if path.starts_with("/backend-api/codex/") => "/backend-api/codex/*",
        _ if path.starts_with("/v1internal") => "/v1internal/*",
        _ => "/v1beta/*",
    }
}

fn identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 192 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
}

fn header_id<'a>(headers: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| headers.get(*name)?.to_str().ok()).filter(|s| identifier(s))
}

fn connection_headers(headers: &HeaderMap) -> Vec<&str> {
    headers.get_all(header::CONNECTION).iter().filter_map(|v| v.to_str().ok()).flat_map(|s| s.split(',').map(str::trim)).collect()
}

fn request_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "content-type"
            | "content-encoding"
            | "user-agent"
            | "anthropic-version"
            | "openai-beta"
            | "originator"
            | "x-goog-api-client"
            | "x-client-version"
            | "x-stainless-lang"
            | "x-stainless-package-version"
            | "x-stainless-os"
            | "x-stainless-arch"
            | "x-stainless-runtime"
            | "x-stainless-runtime-version"
            | "x-stainless-timeout"
            | "session_id"
            | "conversation_id"
    )
}

pub(crate) fn upstream(proxy: &Proxy, account: &Account, uri: &axum::http::Uri) -> Option<reqwest::Url> {
    let path = uri.path();
    let native = path.starts_with("/backend-api/codex/");
    let base = match account.base.as_ref() {
        Some(base) => base.clone(),
        None => match account.lane {
            Lane::Claude => proxy.config.anthropic.clone(),
            Lane::Codex if native && !account.api_key => proxy.config.codex.clone(),
            Lane::Codex => proxy.config.openai.clone(),
            Lane::Google => proxy.config.antigravity.clone(),
            Lane::Go => reqwest::Url::parse("https://opencode.ai/zen/go").ok()?,
        },
    };
    let prefix = base.path().trim_end_matches('/');
    let routed = if native {
        let tail = path.strip_prefix("/backend-api/codex")?;
        if !account.api_key && account.base.is_none() && prefix.is_empty() {
            path.to_owned()
        } else if prefix.ends_with("/v1") || (!account.api_key && account.base.is_none()) {
            format!("{prefix}{tail}")
        } else {
            format!("{prefix}/v1{tail}")
        }
    } else if prefix.ends_with("/v1") && path.starts_with("/v1/") {
        format!("{prefix}{}", path.strip_prefix("/v1")?)
    } else {
        format!("{prefix}{path}")
    };
    let mut url = base;
    url.set_path(&routed);
    // Reject any path whose URL parser would normalize away a routed segment.
    if url.path() != routed {
        return None;
    }
    url.set_query(uri.query());
    Some(url)
}

fn cooldown(headers: &HeaderMap, fallback: u64) -> DateTime<Utc> {
    let now = Utc::now();
    let retry_after = headers.get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|raw| {
        raw.parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .map(|n| n.clamp(1, 604800))
            .or_else(|| DateTime::parse_from_rfc2822(raw).ok().map(|at| (at.with_timezone(&Utc) - now).num_seconds()))
    });
    let limits = headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            if !(name.starts_with("anthropic-ratelimit-") || name.starts_with("x-ratelimit-"))
                || !(name.ends_with("-reset") || name.contains("-reset-"))
            {
                return None;
            }
            let raw = value.to_str().ok()?;
            if let Ok(at) = raw.parse::<DateTime<Utc>>() {
                return Some((at - now).num_seconds());
            }
            if let Ok(n) = raw.parse::<i64>() {
                return Some(if n > 1_000_000_000 { n.saturating_sub(now.timestamp()) } else { n });
            }
            if let Some(ms) = raw.strip_suffix("ms").and_then(|n| n.parse::<f64>().ok()) {
                return Some((ms / 1000.0).ceil() as i64);
            }
            let mut number = String::new();
            let mut seconds = 0.0;
            for ch in raw.chars() {
                if ch.is_ascii_digit() || ch == '.' {
                    number.push(ch);
                    continue;
                }
                let unit = match ch {
                    's' => 1.0,
                    'm' => 60.0,
                    'h' => 3600.0,
                    'd' => 86400.0,
                    _ => return None,
                };
                seconds += number.parse::<f64>().ok()? * unit;
                number.clear();
            }
            number.is_empty().then_some(seconds.ceil() as i64)
        })
        .filter(|n| *n > 0);
    let secs = retry_after.filter(|n| *n > 0).or_else(|| limits.max()).unwrap_or(fallback as i64).clamp(1, 604800);
    now + TimeDelta::seconds(secs)
}

async fn result(proxy: &Proxy, account: &Account, cooldown: Option<DateTime<Utc>>, success: bool) {
    let mut state = proxy.state.write().await;
    let Some(current) = state
        .pool
        .accounts
        .get_mut(&account.id)
        .filter(|a| a.lane == account.lane && (a.access == account.access || (a.refresh.is_some() && a.refresh == account.refresh)))
    else {
        return;
    };
    current.last_used = Some(Utc::now());
    if success {
        current.success = current.success.saturating_add(1);
    } else {
        current.failure = current.failure.saturating_add(1);
    }
    if let Some(until) = cooldown {
        current.cooldown = Some(current.cooldown.map_or(until, |old| old.max(until)));
    }
}

fn millis(duration: Duration) -> i64 {
    duration.as_millis().min(i64::MAX as u128) as i64
}

#[derive(Clone, Copy, Default)]
struct Usage {
    input: i64,
    output: i64,
    cached: i64,
    written: i64,
    thinking: i64,
}

impl Usage {
    fn observe(&mut self, raw: &Value, lane: Lane) {
        let usage = raw
            .get("usage")
            .or_else(|| raw.pointer("/message/usage"))
            .or_else(|| raw.pointer("/response/usage"))
            .or_else(|| raw.pointer("/response/usageMetadata"))
            .or_else(|| raw.get("usageMetadata"))
            .unwrap_or(raw);
        let get = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| if k.starts_with('/') { usage.pointer(k)?.as_i64() } else { usage.get(k)?.as_i64() })
                .filter(|n| *n >= 0)
        };
        let update = |old: &mut i64, names: &[&str]| {
            if let Some(n) = get(names) {
                *old = (*old).max(n);
            }
        };
        update(
            &mut self.cached,
            &[
                "cache_read_input_tokens",
                "/input_tokens_details/cached_tokens",
                "/prompt_tokens_details/cached_tokens",
                "cachedContentTokenCount",
            ],
        );
        update(&mut self.written, &["cache_creation_input_tokens"]);
        update(&mut self.input, &["input_tokens", "prompt_tokens", "promptTokenCount"]);
        update(&mut self.output, &["output_tokens", "completion_tokens", "candidatesTokenCount"]);
        update(
            &mut self.thinking,
            &["/output_tokens_details/reasoning_tokens", "/completion_tokens_details/reasoning_tokens", "thoughtsTokenCount"],
        );
        if lane == Lane::Google
            && let Some(total) = get(&["totalTokenCount"])
        {
            self.output = self.output.max(total.saturating_sub(self.input));
        }
    }

    fn total(self, lane: Lane) -> i64 {
        if lane == Lane::Claude { self.input.saturating_add(self.cached).saturating_add(self.written) } else { self.input }
    }
}

struct Wire {
    proxy: Arc<Proxy>,
    guard: RecordOnDrop,
    account: Account,
    status: StatusCode,
    started: Instant,
    upstream: reqwest::Response,
    first: Option<Bytes>,
    sse: bool,
    usage: Usage,
    usage_buf: Vec<u8>,
    discard_line: bool,
    capture_buf: Vec<u8>,
    response_size: usize,
}

impl Wire {
    async fn next(mut self) -> Result<Option<(Bytes, Self)>, reqwest::Error> {
        let chunk = match self.first.take() {
            Some(chunk) => Ok(Some(chunk)),
            None => self.upstream.chunk().await,
        };
        if let Ok(Some(data)) = &chunk {
            self.response_size = self.response_size.saturating_add(data.len());
            if let Some(exchange) = &mut self.guard.0
                && let Some(capture) = &exchange.capture
            {
                let accepted = capture.enabled(&self.proxy.state.read().await.captures);
                if !accepted {
                    exchange.capture = None;
                    self.capture_buf.clear();
                } else if self.response_size <= CAPTURE_BYTES {
                    self.capture_buf.extend_from_slice(data);
                }
            }
            if self.sse {
                for chunk in data.split_inclusive(|b| *b == b'\n') {
                    if chunk.len() + self.usage_buf.len() > 256 * 1024 {
                        self.discard_line = true;
                        self.usage_buf.clear();
                    }
                    if !self.discard_line {
                        self.usage_buf.extend_from_slice(chunk);
                    }
                    if chunk.ends_with(b"\n") {
                        if !self.discard_line {
                            observe_line(&mut self.usage, &self.usage_buf, self.account.lane);
                        }
                        self.usage_buf.clear();
                        self.discard_line = false;
                    }
                }
            } else if self.response_size <= 1024 * 1024 {
                self.usage_buf.extend_from_slice(data);
            } else {
                self.usage_buf.clear();
            }
        }
        match chunk {
            Ok(Some(data)) => Ok(Some((data, self))),
            done => {
                if self.sse {
                    observe_line(&mut self.usage, &self.usage_buf, self.account.lane);
                } else if let Ok(raw) = serde_json::from_slice(&self.usage_buf) {
                    self.usage.observe(&raw, self.account.lane);
                }
                if self.status.is_success() {
                    result(&self.proxy, &self.account, None, done.is_ok()).await;
                }
                if let Some(mut exchange) = self.guard.0.take() {
                    let outcome = if done.is_err() {
                        "stream_error"
                    } else if self.status.is_success() {
                        "success"
                    } else {
                        "http_error"
                    };
                    if let Some(last) = exchange.attempts.last_mut() {
                        last.outcome = outcome.into();
                    }
                    let response =
                        if self.response_size <= CAPTURE_BYTES { transformed_response(&self.capture_buf, self.sse) } else { None };
                    exchange
                        .finish(
                            Some(self.status),
                            outcome,
                            self.usage,
                            Some(millis(self.started.elapsed())),
                            Some((response, self.response_size)),
                        )
                        .await;
                }
                done.map(|_| None)
            }
        }
    }
}

fn observe_line(usage: &mut Usage, line: &[u8], lane: Lane) {
    if let Some(data) = line.strip_prefix(b"data:")
        && let Ok(raw) = serde_json::from_slice::<Value>(data)
    {
        usage.observe(&raw, lane);
    }
}

struct RecordOnDrop(Option<Exchange>);
impl Drop for RecordOnDrop {
    fn drop(&mut self) {
        if let Some(mut exchange) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                if let Some(last) = exchange.attempts.last_mut() {
                    last.outcome = "disconnected".into();
                }
                exchange.finish(None, "disconnected", Usage::default(), None, None).await;
            });
        }
    }
}

struct Capture {
    epoch: Uuid,
    lease_generation: u64,
    method: String,
    path: String,
    request: Option<Vec<u8>>,
    request_size: usize,
    response_type: String,
}

impl Capture {
    fn enabled(&self, captures: &crate::Captures) -> bool {
        captures.enabled() && self.epoch == captures.epoch && self.lease_generation == captures.lease_generation
    }
}

struct Exchange {
    proxy: Arc<Proxy>,
    id: String,
    started: Instant,
    occurred: DateTime<Utc>,
    lane: Lane,
    model: String,
    purpose: ActivityPurpose,
    session: Option<String>,
    parent: Option<String>,
    attempts: Vec<ActivityAttemptIn>,
    capture: Option<Capture>,
}

impl Exchange {
    fn new(proxy: Arc<Proxy>, lane: Lane, model: &str, session: Option<String>, parent: Option<String>, purpose: ActivityPurpose) -> Self {
        Self {
            proxy,
            id: Uuid::new_v4().to_string(),
            started: Instant::now(),
            occurred: Utc::now(),
            lane,
            model: model.into(),
            purpose,
            session,
            parent,
            attempts: vec![],
            capture: None,
        }
    }

    fn attempt(
        &mut self,
        account: &Account,
        previous: Option<&str>,
        at: DateTime<Utc>,
        began: Instant,
        status: Option<StatusCode>,
        outcome: &str,
    ) {
        let index = self.attempts.len();
        self.attempts.push(ActivityAttemptIn {
            source_attempt_id: format!("{}:{index}", self.id),
            attempt_index: index as i32,
            provider_id: self.lane.family().into(),
            account_id: Some(account.hub_id_for(&self.proxy.source_host, &self.proxy.source_user)),
            moved_from_account_id: previous
                .filter(|id| *id != account.id)
                .map(|id| format!("{}:{}:{}:{id}", self.lane.family(), self.proxy.source_host, self.proxy.source_user)),
            requested_model: self.model.clone(),
            effective_model: self.model.clone(),
            effort: None,
            status_code: status.map(|s| i32::from(s.as_u16())),
            outcome: outcome.into(),
            error_message: None,
            latency_ms: Some(millis(began.elapsed())),
            ttft_ms: status.filter(StatusCode::is_success).map(|_| millis(began.elapsed())),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            thinking_tokens: 0,
            fast_mode: false,
            capture_state: CaptureState::MetadataOnly,
            captured_body: None,
            started_at: at,
            finished_at: None,
        });
    }

    fn complete_attempt(&mut self, usage: Usage, latency: Option<i64>) {
        if let Some(last) = self.attempts.last_mut() {
            last.finished_at = Some(Utc::now());
            last.latency_ms = latency.or(last.latency_ms);
            last.input_tokens = usage.total(self.lane);
            last.output_tokens = usage.output;
            last.cache_read_tokens = usage.cached;
            last.cache_write_tokens = usage.written;
            last.thinking_tokens = usage.thinking;
        }
    }

    async fn finish(
        mut self,
        status: Option<StatusCode>,
        outcome: &str,
        usage: Usage,
        latency: Option<i64>,
        response: Option<(Option<Vec<u8>>, usize)>,
    ) {
        self.complete_attempt(usage, latency);
        let mut state = self.proxy.state.write().await;
        let mut capture_state = CaptureState::MetadataOnly;
        if let Some(capture) = self.capture
            && capture.enabled(&state.captures)
        {
            let (response, response_size) = response.unwrap_or_default();
            let complete = capture.request.is_some() && response.is_some() && outcome != "stream_error";
            capture_state = if complete { CaptureState::Transformed } else { CaptureState::Truncated };
            let request = capture.request.unwrap_or_default();
            let response = response.unwrap_or_default();
            if let Some(last) = self.attempts.last_mut() {
                last.capture_state = capture_state;
                if complete
                    && request.len() + response.len() <= 8 * 1024
                    && let (Ok(request), Ok(response)) = (std::str::from_utf8(&request), std::str::from_utf8(&response))
                {
                    // Keep the Hub batch small; larger transformed captures stay in the local ring.
                    last.captured_body = Some(json!({"redacted":true,"request":request,"response":response}));
                }
            }
            let terminal = json!({
                "id":self.id, "traceId":self.id, "startedAt":self.occurred, "method":capture.method, "path":capture.path,
                "event":"end", "frameCount":0,
                "status":status.map_or(0, |s| s.as_u16()), "state":if complete {"complete"} else {"partial"},
                "reason": if complete {"transformed_redacted"} else {"body_unavailable_or_over_limit"},
                "request":"","response":"",
                "requestType":"application/json","responseType":capture.response_type,
                "requestEncoding":"","responseEncoding":"","requestDecoded":true,"responseDecoded":true,"format":"http",
                "requestBytes":request.len(), "responseBytes":response.len(),
                "requestCut":(request.is_empty() && capture.request_size > 0).then(|| json!({"offset":0,"omitted":capture.request_size})),
                "responseCut":(response.is_empty() && response_size > 0).then(|| json!({"offset":0,"omitted":response_size})),
                "requestSha256":hex::encode(Sha256::digest(&request)), "responseSha256":hex::encode(Sha256::digest(&response)),
            });
            let mut start = terminal.clone();
            start["event"] = json!("start");
            start["state"] = json!("pending");
            start["reason"] = json!("capture-open");
            start["requestBytes"] = json!(0);
            start["responseBytes"] = json!(0);
            start["requestSha256"] = json!("");
            start["responseSha256"] = json!("");
            let mut events = vec![start.clone()];
            for (direction, bytes, field) in [("request", &request, "requestBytes"), ("response", &response, "responseBytes")] {
                if !bytes.is_empty() {
                    start["event"] = json!("data");
                    start[field] = json!(bytes.len());
                    start["chunk"] = json!({"direction":direction,"offset":0,"data":STANDARD.encode(bytes),
                        "frame":0,"type":"","at":self.occurred,"end":false});
                    events.push(start.clone());
                }
            }
            events.push(terminal);
            state.captures.push(events);
        }
        let last = self.attempts.last();
        let row = ActivityExchangeIn {
            source_exchange_id: self.id,
            source: ActivitySource::Proxy,
            purpose: self.purpose,
            client: match self.lane {
                Lane::Claude => "claude-code",
                Lane::Codex => "codex",
                Lane::Google => "antigravity",
                Lane::Go => "opencode",
            }
            .into(),
            client_version: None,
            transport: Some("http".into()),
            session_id: self.session,
            parent_edge_kind: self.parent.as_ref().map(|_| TraceEdgeKind::Subagent),
            parent_session_id: self.parent,
            provider_id: self.lane.family().into(),
            account_id: last.and_then(|a| a.account_id.clone()),
            requested_model: self.model.clone(),
            effective_model: self.model,
            effort: None,
            outcome: outcome.into(),
            status_code: status.map(|s| i32::from(s.as_u16())),
            error_message: None,
            latency_ms: Some(millis(self.started.elapsed())),
            ttft_ms: last.and_then(|a| a.ttft_ms),
            input_tokens: usage.total(self.lane),
            output_tokens: usage.output,
            cache_read_tokens: usage.cached,
            cache_write_tokens: usage.written,
            thinking_tokens: usage.thinking,
            fast_mode: false,
            capture_state,
            searchable_text: None,
            occurred_at: self.occurred,
            attempts: self.attempts,
        };
        if state.activity.len() == ACTIVITY_CAP {
            state.activity.pop_front();
        }
        state.activity.push_back(row);
    }
}

/// Capture structure and usage, never arbitrary text, object keys, or numbers outside
/// known usage fields: provider/user content can contain secrets in any position.
fn transformed(raw: &Value) -> Option<Vec<u8>> {
    const FIELDS: &[&str] = &[
        "type",
        "id",
        "model",
        "role",
        "message",
        "messages",
        "content",
        "text",
        "input",
        "output",
        "delta",
        "response",
        "metadata",
        "usage",
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "stream",
        "stop_reason",
        "error",
        "tool_calls",
        "tool_use",
        "tools",
        "arguments",
    ];
    fn redact(raw: &Value, numeric: bool) -> Value {
        match raw {
            Value::Object(obj) => Value::Object(
                obj.iter()
                    .filter(|(key, _)| FIELDS.contains(&key.as_str()))
                    .map(|(k, v)| (k.clone(), redact(v, k.ends_with("_tokens"))))
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.iter().map(|v| redact(v, false)).collect()),
            Value::String(_) => Value::String("[REDACTED]".into()),
            Value::Number(_) if !numeric => Value::Null,
            _ => raw.clone(),
        }
    }
    let data = serde_json::to_vec(&redact(raw, false)).ok()?;
    (data.len() <= CAPTURE_BYTES).then_some(data)
}

fn transformed_response(bytes: &[u8], sse: bool) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    if !sse {
        return transformed(&serde_json::from_slice(bytes).ok()?);
    }
    let mut output = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        let Some(data) = line.strip_prefix(b"data:") else { continue };
        if let Ok(raw) = serde_json::from_slice::<Value>(data) {
            output.extend_from_slice(b"data: ");
            output.extend(transformed(&raw)?);
            output.extend_from_slice(b"\n\n");
            if output.len() > CAPTURE_BYTES {
                return None;
            }
        }
    }
    (!output.is_empty()).then_some(output)
}
