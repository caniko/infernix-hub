use std::{
    ops::Range,
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
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use contracts::providers::{ActivityAttemptIn, ActivityExchangeIn, ActivityPurpose, ActivitySource, CaptureState, TraceEdgeKind};
use futures_util::stream;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    ACTIVITY_CAP, CAPTURE_BYTES, Proxy,
    config::matches_secret,
    openai_account,
    pool::{Account, Cooldown, Lane, MAX_COOLDOWN_SECS, Pool, bounded_identifier, model_cooldown_scope, text},
};

pub(crate) const POOL_QUOTA_HEADER: &str = "x-fleetix-pool-quota";

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

fn lane_error(lane: Lane, status: StatusCode, message: &'static str) -> Response {
    if lane == Lane::Google {
        let rpc_status = match status {
            StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
            StatusCode::FORBIDDEN => "PERMISSION_DENIED",
            StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNSUPPORTED_MEDIA_TYPE => "INVALID_ARGUMENT",
            StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
            StatusCode::GATEWAY_TIMEOUT => "DEADLINE_EXCEEDED",
            _ => "UNAVAILABLE",
        };
        return (
            status,
            axum::Json(json!({
                "error": {
                    "code": status.as_u16(),
                    "message": message,
                    "status": rpc_status,
                }
            })),
        )
            .into_response();
    }
    (status, axum::Json(json!({"error":{"message":message}}))).into_response()
}

struct RetainedQuota {
    account: Account,
    until: DateTime<Utc>,
    reader: Option<tokio::task::JoinHandle<DateTime<Utc>>>,
}

struct PoolQuota {
    reset: DateTime<Utc>,
    verified_reset: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
}

impl PoolQuota {
    fn unverified(reset: DateTime<Utc>) -> Self {
        Self { reset, verified_reset: None, now: Utc::now() }
    }
}

enum QuotaSnapshot {
    Ready(PoolQuota),
    ReadBody(usize, tokio::task::JoinHandle<DateTime<Utc>>),
}

#[cfg(test)]
tokio::task_local! {
    pub(super) static AFTER_FAILOVER_PREVIEW: fn(&mut crate::pool::Pool);
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
        return lane_error(lane, StatusCode::UNAUTHORIZED, "invalid inference credentials");
    }
    if parts.method == axum::http::Method::CONNECT || parts.method == axum::http::Method::TRACE {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if lane == Lane::Google && !inspect_google(&parts.headers) {
        return lane_error(lane, StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported Google request content encoding");
    }
    let Ok(body) = to_bytes(body, 32 * 1024 * 1024).await else {
        return lane_error(lane, StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
    };
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(raw) => raw,
        Err(_) if lane == Lane::Google && !body.trim_ascii().is_empty() => {
            return lane_error(lane, StatusCode::BAD_REQUEST, "invalid Google request JSON");
        }
        Err(_) => Value::Null,
    };
    let mut projects = ProjectFields::default();
    let cache = if lane == Lane::Google {
        if raw.is_object() {
            let Some(fields) = ProjectFields::scan(&body) else {
                return lane_error(lane, StatusCode::BAD_REQUEST, "invalid or duplicate Google request field");
            };
            projects = fields;
        }
        match resolve_google_cache(&raw) {
            Ok(cache) => cache,
            Err(message) => return lane_error(lane, StatusCode::BAD_REQUEST, message),
        }
    } else {
        None
    };
    let required_project = cache.map(|(_, project)| project);
    let lane = if lane == Lane::Codex && path == "/v1/chat/completions" && text(&raw, &["model"]).is_some_and(proxy_model_is_go) {
        Lane::Go
    } else {
        lane
    };
    let path_model = || {
        let (model, _) = path.strip_prefix("/v1beta/models/")?.split_once(':')?;
        (!model.contains('/')).then_some(model)
    };
    let model = path_model()
        .or_else(|| text(&raw, &["model", "/request/model"]))
        .map(|m| if lane == Lane::Google { m.strip_prefix("models/").unwrap_or(m) } else { m })
        .filter(|s| bounded_identifier(s))
        .unwrap_or_default();
    let effort = text(&raw, &["/reasoning/effort", "reasoning_effort", "/output_config/effort", "effort"])
        .filter(|s| bounded_identifier(s))
        .map(str::to_owned);
    let metadata_user: Value = text(&raw, &["/metadata/user_id"]).and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
    let session_id = |s: &str| {
        let digits = s.strip_prefix('-').unwrap_or(s);
        bounded_identifier(s) && (lane != Lane::Google || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()))
    };
    let session = if lane == Lane::Google {
        extract_google_session(&parts.headers, &raw)
    } else {
        header_id(&parts.headers, &["x-session-id", "x-claude-session-id", "session_id"])
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
            .filter(|s| session_id(s))
            .map(str::to_owned)
    };
    let parent_header = header_id(&parts.headers, &["x-parent-session-id", "x-claude-parent-session-id"]);
    let parent = parent_header
        .or_else(|| text(&raw, &["/metadata/parent_session_id", "/metadata/parentSessionId"]))
        .or_else(|| text(&metadata_user, &["parent_session_id"]))
        .filter(|s| session_id(s) && Some(*s) != session.as_deref())
        .map(str::to_owned);
    let purpose = match parts.headers.get("x-cpa-purpose").and_then(|v| v.to_str().ok()) {
        Some("window-start") => ActivityPurpose::WindowStarter,
        Some("canary") => ActivityPurpose::Canary,
        Some("thinking-test") => ActivityPurpose::ThinkingTest,
        Some("eval-replay") => ActivityPurpose::EvalReplay,
        _ => ActivityPurpose::Ordinary,
    };
    let mut exchange = Exchange::new(proxy.clone(), lane, model, effort, session, parent, purpose);
    if lane == Lane::Google && exchange.affinity_key.is_none() {
        exchange.affinity_key = google_affinity_hint(&raw, model, cache.map(|(resource, _)| resource));
    }
    let (policy, moved_from, capture) = {
        let state = proxy.state.read().await;
        (
            state.pool.policy.clone(),
            exchange.affinity_key.as_deref().and_then(|s| state.pool.bound(lane, s)).map(str::to_owned),
            state.captures.enabled().then_some((state.captures.epoch, state.captures.lease_generation)),
        )
    };
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
    let previous_session_account = moved_from.as_deref().filter(|_| exchange.session.is_some());
    let mut retained_quota = Vec::new();
    let mut last_decoded_quota_reset = None;
    let quota_snapshot = |pool: &Pool, now, retained: &mut [RetainedQuota]| {
        if lane != Lane::Google {
            let reset = pool
                .next_reset(lane, model, required_project, now)
                .filter(|_| pool.all_quota_limited(lane, model, required_project, now))?;
            return Some(QuotaSnapshot::Ready(PoolQuota { reset, verified_reset: None, now }));
        }
        let live_deadline = |account: &Account| account.quota_limited_until(now, model);
        let supported = |account: &Account| pool.supports(account, lane, model, required_project);
        let reset = pool
            .accounts
            .values()
            .filter(|account| supported(account))
            .filter_map(|account| {
                live_deadline(account).or_else(|| {
                    retained
                        .iter()
                        .find(|quota| quota.account.id == account.id && account.same_credential(&quota.account))
                        .map(|quota| quota.until)
                })
            })
            .min()?;
        let verified_reset = pool.verified_google_reset_at(model, required_project, now);
        let pending = retained.iter_mut().enumerate().find_map(|(index, quota)| {
            let current = pool.accounts.get(&quota.account.id).filter(|a| a.same_credential(&quota.account) && supported(a))?;
            if live_deadline(current).unwrap_or(quota.until) > reset.max(verified_reset.unwrap_or(reset)) {
                return None;
            }
            Some(QuotaSnapshot::ReadBody(index, quota.reader.take()?))
        });
        pending.or(Some(QuotaSnapshot::Ready(PoolQuota { reset, verified_reset, now })))
    };
    let exhausted_reset = async |mut snapshot, retained: &mut [RetainedQuota]| {
        loop {
            let (index, reader) = match snapshot? {
                QuotaSnapshot::Ready(quota) => return Some(quota),
                QuotaSnapshot::ReadBody(index, reader) => (index, reader),
            };
            if let Ok(until) = reader.await {
                retained[index].until = until;
            }
            // A body reader can extend backoff; recheck the live route after joining it.
            let state = proxy.state.read().await;
            snapshot = quota_snapshot(&state.pool, Utc::now(), retained);
        }
    };
    loop {
        let (account, snapshot) = {
            let mut state = proxy.state.write().await;
            let now = Utc::now();
            let account =
                state.pool.pick(lane, (exchange.affinity_key.as_deref(), exchange.parent.as_deref()), model, required_project, &tried, now);
            let snapshot = if account.is_none() { quota_snapshot(&state.pool, now, &mut retained_quota) } else { None };
            (account, snapshot)
        };
        let Some(mut account) = account else {
            if let Some(quota) =
                exhausted_reset(snapshot, &mut retained_quota).await.or_else(|| last_decoded_quota_reset.map(PoolQuota::unverified))
            {
                return exchange.rate_limited(quota, Usage::default(), None).await;
            }
            exchange.finish(Some(last_error), "unavailable", Usage::default(), None, None).await;
            return lane_error(lane, last_error, "no eligible upstream account");
        };
        tried.push(account.id.clone());
        let began = Instant::now();
        let started = Utc::now();
        let timeout = Duration::from_secs(policy.first_byte_timeout_secs.clamp(1, 600));
        let deadline = tokio::time::Instant::now() + timeout;
        let reload_on_401 = !account.api_key && matches!(lane, Lane::Claude | Lane::Codex);
        let incoming = async {
            let url = upstream(&proxy, &account, &parts.uri).ok_or(())?;
            let build = |account: &Account| {
                let payload = request_body(&body, &projects, account);
                let mut req = proxy.http.request(parts.method.clone(), url.clone()).body(payload);
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
                    Lane::Claude if account.api_key => req.header("x-api-key", &account.access),
                    Lane::Claude => {
                        let beta = parts.headers.get("anthropic-beta").and_then(|v| v.to_str().ok()).unwrap_or_default();
                        let beta = if beta.split(',').any(|item| item.trim() == "oauth-2025-04-20") {
                            beta.to_owned()
                        } else if beta.is_empty() {
                            "oauth-2025-04-20".into()
                        } else {
                            format!("{beta},oauth-2025-04-20")
                        };
                        req.header("anthropic-beta", beta).bearer_auth(&account.access)
                    }
                    Lane::Codex => openai_account(req.bearer_auth(&account.access), account),
                    // Inspect Google's frames without modifying other providers' compression negotiation.
                    Lane::Google if account.api_key => {
                        req.header(header::ACCEPT_ENCODING, "identity").header("x-goog-api-key", &account.access)
                    }
                    Lane::Google => req.header(header::ACCEPT_ENCODING, "identity").bearer_auth(&account.access),
                    _ => req.bearer_auth(&account.access),
                }
            };
            let resolved = proxy.token_account(&account, false).await.map_err(|_| ())?;
            if required_project.is_some_and(|project| resolved.project.as_deref() != Some(project)) {
                return Err(());
            }
            account = resolved;
            let mut res = build(&account).send().await.map_err(|_| ())?;
            if res.status() == StatusCode::UNAUTHORIZED
                && (reload_on_401 || (lane == Lane::Google && !account.api_key && account.refresh.is_some()))
                && let Ok(fresh) = proxy.token_account(&account, true).await
                && required_project.is_none_or(|project| fresh.project.as_deref() == Some(project))
            {
                account = fresh;
                res = build(&account).send().await.map_err(|_| ())?;
            }
            // A known client error must survive an upstream body failure or stall.
            let first = if res.status().is_client_error() { None } else { res.chunk().await.map_err(|_| ())? };
            Ok::<_, ()>((res, first))
        };
        let (mut response, first) = match tokio::time::timeout_at(deadline, incoming).await {
            Ok(Ok(result)) => result,
            failure => {
                let timed_out = failure.is_err();
                last_error = if timed_out { StatusCode::GATEWAY_TIMEOUT } else { StatusCode::BAD_GATEWAY };
                exchange.attempt(
                    &account,
                    previous_session_account,
                    started,
                    began,
                    None,
                    if timed_out { "first_byte_timeout" } else { "upstream_error" },
                );
                result(&proxy, &account, model, None, false).await;
                exchange.complete_attempt(Usage::default(), None);
                continue;
            }
        };
        let headers = response.headers().clone();
        let sse = is_sse(&headers);
        let google_stream = lane == Lane::Google && response.status() == StatusCode::OK && inspect_google(&headers);
        let mut prelude = GooglePrelude { first: [first, None], ..GooglePrelude::default() };
        if google_stream && let Some(first) = prelude.first[0].take() {
            prelude = google_first(&mut response, first, sse).await;
        }
        let status = prelude.error.as_ref().map_or_else(|| response.status(), |(error, _)| error.status);
        let limited = status == StatusCode::TOO_MANY_REQUESTS;
        let received_at = Utc::now();
        let decoded_quota_reset = prelude
            .error
            .as_ref()
            .filter(|_| limited)
            .map(|(error, _)| quota_hint(lane, &headers, error.reset, received_at).map_or(received_at, |hint| hint.until));
        let mut pending_quota_body = None;
        let cooldown = if let Some((error, _)) = &prelude.error {
            error.backoff(&headers, policy.failover_cooldown_secs, received_at)
        } else if limited {
            let header = header_cooldown(lane, &headers, received_at);
            let body_reset = if lane == Lane::Google && header.is_none() {
                let body = read_google_quota_body(&mut response, deadline).await;
                prelude.first = body.chunks;
                prelude.stream_error = body.error;
                body.reset
            } else {
                pending_quota_body = header.filter(|_| lane == Lane::Google);
                None
            };
            let until = quota_deadline(merge_quota_hints(&headers, header, body_reset), policy.failover_cooldown_secs, Utc::now());
            Some(Backoff::Quota(until))
        } else if status == StatusCode::UNAUTHORIZED {
            Some(auth_backoff(policy.failover_cooldown_secs))
        } else {
            None
        };
        let mut quota_until = match cooldown {
            Some(Backoff::Quota(cooldown)) if lane == Lane::Google => Some(cooldown.until),
            _ => None,
        };
        let failover = limited || status == StatusCode::UNAUTHORIZED || (status.is_server_error() && policy.switch_on_server_error);
        exchange.attempt(
            &account,
            previous_session_account,
            started,
            began,
            Some(status),
            if status.is_success() {
                "success"
            } else if limited && lane == Lane::Google {
                "rate_limited"
            } else {
                "http_error"
            },
        );
        if !status.is_success() {
            result(&proxy, &account, model, cooldown, false).await;
        }
        if limited {
            proxy.schedule_usage_refresh(account.id.clone()).await;
        }
        if failover {
            let has_next = {
                let mut state = proxy.state.write().await;
                let now = Utc::now();
                // Preview on the real scheduler; retries do not increment the round-robin cursor.
                let has_next = state
                    .pool
                    .pick(lane, (exchange.affinity_key.as_deref(), exchange.parent.as_deref()), model, required_project, &tried, now)
                    .is_some();
                #[cfg(test)]
                if has_next {
                    let _ = AFTER_FAILOVER_PREVIEW.try_with(|hook| hook(&mut state.pool));
                }
                has_next
            };
            if has_next {
                last_decoded_quota_reset = decoded_quota_reset.or(last_decoded_quota_reset);
                let reader = pending_quota_body.map(|header_until| {
                    let proxy = proxy.clone();
                    let model = model.to_owned();
                    let account = account.clone();
                    tokio::spawn(async move {
                        let body = read_google_quota_body(&mut response, deadline).await;
                        let merged = merge_quota_hints(&headers, Some(header_until), body.reset).unwrap_or(header_until);
                        extend_quota(&proxy, &account, &model, (merged != header_until).then_some(merged)).await;
                        merged.until
                    })
                });
                if let Some(until) = quota_until {
                    retained_quota.push(RetainedQuota { account, until, reader });
                }
                exchange.complete_attempt(prelude.usage, Some(millis(began.elapsed())));
                proxy.log("warn", "upstream request failed over").await;
                continue;
            }
            if let Some(header_until) = pending_quota_body {
                let body = read_google_quota_body(&mut response, deadline).await;
                let merged = merge_quota_hints(&headers, Some(header_until), body.reset).unwrap_or(header_until);
                extend_quota(&proxy, &account, model, (merged != header_until).then_some(merged)).await;
                quota_until = Some(merged.until);
                prelude.first = body.chunks;
                prelude.stream_error = body.error;
            }
            if let Some(until) = quota_until {
                retained_quota.push(RetainedQuota { account: account.clone(), until, reader: None });
            }
            let snapshot = {
                let state = proxy.state.read().await;
                quota_snapshot(&state.pool, Utc::now(), &mut retained_quota)
            };
            if let Some(reset) = exhausted_reset(snapshot, &mut retained_quota).await {
                return exchange.rate_limited(reset, prelude.usage, Some(millis(began.elapsed()))).await;
            }
        }
        if let Some((error, raw)) = prelude.error {
            if let Some(reset) = decoded_quota_reset {
                return exchange.rate_limited(PoolQuota::unverified(reset), prelude.usage, Some(millis(began.elapsed()))).await;
            }
            let bytes = raw.to_string();
            if let Some(capture) = &mut exchange.capture {
                capture.response_type = "application/json".into();
            }
            exchange
                .finish(Some(status), error.outcome(), prelude.usage, Some(millis(began.elapsed())), Some((transformed(&raw), bytes.len())))
                .await;
            return (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response();
        }
        if status.is_success() {
            let mut state = proxy.state.write().await;
            if let Some(key) = &exchange.affinity_key {
                let current = state.pool.bound(lane, key);
                // Do not overwrite a conversation moved while this request was in flight.
                if current.is_none() || current == moved_from.as_deref() || current == Some(&account.id) {
                    state.pool.bind(lane, key, &account.id);
                    if let (Some(session), Some(parent)) = (&exchange.session, &exchange.parent) {
                        state.pool.link(lane, session, parent);
                    }
                }
            }
        }
        if let Some(capture) = &mut exchange.capture {
            capture.response_type = if sse { "text/event-stream" } else { "application/json" }.into();
        }
        let state = Wire {
            proxy: proxy.clone(),
            guard: RecordOnDrop(Some(exchange), None),
            account,
            status,
            started: began,
            upstream: response,
            first: prelude.first,
            stream_error: prelude.stream_error,
            flush_google_prefix: prelude.flush_prefix,
            upstream_done: false,
            sse,
            google: (google_stream && status.is_success()).then(|| GoogleFrames::new(sse)),
            fallback: policy.failover_cooldown_secs,
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
            if name == POOL_QUOTA_HEADER {
                continue; // Only a locally synthesized pool response can attest a reset.
            }
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

fn google_quota_message(model: &str) -> String {
    if model.is_empty() { "You have exhausted your capacity.".to_owned() } else { format!("Quota exhausted for model {model}.") }
}

const GOOGLE_QUOTA_DELAY_MARGIN_SECS: i64 = 5;

fn google_quota_detail(model: &str) -> Value {
    let mut detail = json!({
        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
        "reason": "QUOTA_EXHAUSTED",
        "domain": "cloudcode-pa.googleapis.com",
    });
    if !model.is_empty() {
        detail["metadata"] = json!({"model":model});
    }
    detail
}

fn google_quota_error_body(model: &str) -> Value {
    json!({
        "error": {
            "code": 429,
            "message": "all eligible accounts are rate limited",
            "status": "RESOURCE_EXHAUSTED",
            "details": [google_quota_detail(model)],
        }
    })
}

pub(crate) fn normalize_google_quota(raw: &mut Value, model: &str, now: DateTime<Utc>) {
    if let Value::Array(items) = raw {
        if let Some(item) = items.iter_mut().find(|item| GoogleError::from_frame(item).is_some()) {
            normalize_google_quota(item, model, now);
        }
        return;
    }
    let Some(error) = raw.get_mut("error").and_then(Value::as_object_mut) else {
        return;
    };
    error.insert("code".into(), json!(429));
    error.insert("status".into(), json!("RESOURCE_EXHAUSTED"));
    if error.get("message").and_then(Value::as_str).is_none_or(|message| message.trim().is_empty()) {
        error.insert("message".into(), Value::String(google_quota_message(model)));
    }
    let details = error.entry("details").or_insert(Value::Null);
    if !details.is_array() {
        *details = json!([]);
    }
    let Some(details) = details.as_array_mut() else {
        return;
    };
    // Keep usable upstream timing on committed frames; never replace it with local backoff.
    for detail in details.iter_mut() {
        match google_detail_kind(detail) {
            Some("google.rpc.ErrorInfo" | "ErrorInfo") => {
                if let Some(metadata) = detail.get_mut("metadata").and_then(Value::as_object_mut) {
                    metadata.retain(|name, value| match name.as_str() {
                        "quotaResetTimeStamp" => google_reset_timestamp(value, now).is_some_and(|hint| hint.verified_reset),
                        "quotaResetDelay" => google_reset_delay(value, now).is_some_and(|hint| hint.verified_reset),
                        _ => true,
                    });
                }
            }
            Some("google.rpc.RetryInfo" | "RetryInfo") => {
                if !google_reset_delay(&detail["retryDelay"], now).is_some_and(|hint| hint.verified_reset)
                    && let Some(detail) = detail.as_object_mut()
                {
                    detail.remove("retryDelay");
                }
            }
            _ => {}
        }
    }
    if !details.iter().any(|detail| matches!(google_detail_kind(detail), Some("google.rpc.ErrorInfo" | "ErrorInfo"))) {
        details.push(google_quota_detail(model));
    }
}

fn pool_limit(lane: Lane, model: &str, PoolQuota { reset, verified_reset, now }: PoolQuota) -> Response {
    let payload = if lane == Lane::Google {
        google_quota_error_body(model)
    } else {
        json!({
            "type":"error","error":{"type":"usage_limit_reached","message":"all eligible accounts are rate limited"}
        })
    };
    let mut response = (StatusCode::TOO_MANY_REQUESTS, axum::Json(payload)).into_response();
    let headers = response.headers_mut();
    headers.insert("x-should-retry", header::HeaderValue::from_static("false"));
    if lane == Lane::Google
        && bounded_identifier(model)
        && let Some(at) = verified_reset
        && let Some(at) = DateTime::from_timestamp(at.timestamp() + i64::from(at.timestamp_subsec_nanos() > 0), 0)
        && at > now
        && at - now <= TimeDelta::seconds(MAX_COOLDOWN_SECS)
        && let Ok(value) =
            header::HeaderValue::from_str(&format!("v1;retry_at={};wire_model={model}", at.to_rfc3339_opts(SecondsFormat::Secs, true)))
    {
        headers.insert(POOL_QUOTA_HEADER, value);
    }
    let set = |headers: &mut HeaderMap, name: &'static str, n: i64| {
        if let Ok(value) = header::HeaderValue::from_str(&n.to_string()) {
            headers.insert(name, value);
        }
    };
    if reset != DateTime::<Utc>::MAX_UTC {
        let minimum = if lane == Lane::Google { GOOGLE_QUOTA_DELAY_MARGIN_SECS } else { 1 };
        set(headers, "retry-after", (reset - now).num_seconds().max(minimum));
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

#[test]
fn pool_witness_rounds_up_without_exceeding_the_horizon() {
    let now = "2026-11-23T12:00:00.900Z".parse::<DateTime<Utc>>().unwrap();
    let blocker = now + TimeDelta::seconds(10);
    let model = "gemini-neon";
    let response = pool_limit(Lane::Google, model, PoolQuota { reset: blocker, verified_reset: Some(blocker), now });
    let expected = "2026-11-23T12:00:11Z";
    assert_eq!(response.headers()[POOL_QUOTA_HEADER].to_str().unwrap(), format!("v1;retry_at={expected};wire_model={model}"));
    assert!(expected.parse::<DateTime<Utc>>().unwrap() >= blocker);
    assert_eq!(response.headers()["retry-after"], "10");

    let horizon = now + TimeDelta::days(7);
    let response = pool_limit(Lane::Google, model, PoolQuota { reset: horizon, verified_reset: Some(horizon), now });
    assert!(!response.headers().contains_key(POOL_QUOTA_HEADER));
}

#[test]
fn google_near_expiry_reset_keeps_transport_backoff_without_a_body_delay() {
    let now = DateTime::from_timestamp_millis(1_800_000_000_999).unwrap();
    let reset = now + TimeDelta::milliseconds(2);
    let response = pool_limit(Lane::Google, "gemini", PoolQuota { reset, verified_reset: None, now });
    assert_eq!(response.headers()["retry-after"], "5");
    assert!(!response.headers().contains_key(POOL_QUOTA_HEADER));
}

pub(crate) async fn run_window_start(
    proxy: Arc<Proxy>,
    account: &Account,
    model: &str,
    request: reqwest::RequestBuilder,
) -> Result<(), ()> {
    let mut exchange = Exchange::new(proxy.clone(), account.lane, model, None, None, None, ActivityPurpose::WindowStarter);
    let began = Instant::now();
    let started = Utc::now();
    let response = request.timeout(Duration::from_secs(45)).send().await;
    let mut usage = Usage::default();
    let (status, outcome, limited) = if let Ok(mut response) = response {
        let status = response.status();
        let limited = if status == StatusCode::TOO_MANY_REQUESTS {
            let fallback = proxy.state.read().await.pool.policy.failover_cooldown_secs;
            let now = Utc::now();
            let header = header_cooldown(account.lane, response.headers(), now);
            let body = if account.lane == Lane::Google {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
                read_google_quota_body(&mut response, deadline).await.reset
            } else {
                None
            };
            Some(Backoff::Quota(quota_deadline(merge_quota_hints(response.headers(), header, body), fallback, Utc::now())))
        } else {
            None
        };
        let sse = is_sse(response.headers());
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
    result(&proxy, account, model, limited, success).await;
    if limited.is_some() {
        proxy.schedule_usage_refresh(account.id.clone()).await;
    }
    exchange.finish(status, outcome, usage, Some(millis(began.elapsed())), None).await;
    if success { Ok(()) } else { Err(()) }
}

fn extract_google_session(headers: &HeaderMap, raw: &Value) -> Option<String> {
    let valid = |s: &str| {
        let digits = s.strip_prefix('-').unwrap_or(s);
        bounded_identifier(s) && (digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()))
    };
    // ACP's numeric request.session_id is a shared user hash, not a conversation ID.
    ["x-session-id", "session_id", "conversation_id"]
        .into_iter()
        .filter_map(|name| headers.get(name)?.to_str().ok())
        .chain(
            [
                "trajectoryId",
                "session_id",
                "sessionId",
                "conversation_id",
                "conversationId",
                "/request/session_id",
                "/request/sessionId",
                "/request/conversation_id",
                "/request/conversationId",
                "/metadata/session_id",
                "/metadata/sessionId",
                "/metadata/conversation_id",
                "/metadata/conversationId",
            ]
            .into_iter()
            .filter_map(|key| text(raw, &[key])),
        )
        .chain(
            ["user_prompt_id", "/request/user_prompt_id"]
                .into_iter()
                .filter_map(|key| text(raw, &[key])?.split_once("########").map(|(conv, _)| conv)),
        )
        .find(|s| valid(s))
        .map(str::to_owned)
}

#[cfg(test)]
#[test]
fn proxy_affinity_hints_do_not_create_google_sessions() {
    let headers = HeaderMap::new();
    let mut raw = json!({"request":{"session_id":"-123", "contents":[{"role":"user","parts":[{"text":"same prompt"}]}]}});
    let hint = google_affinity_hint(&raw, "gemini", None).unwrap();
    assert!(hint.starts_with("#ag-conv-"));
    for id in ["prompt-one", "prompt-two", "123########prompt"] {
        raw["user_prompt_id"] = json!(id);
        assert_eq!(google_affinity_hint(&raw, "gemini", None).as_deref(), Some(hint.as_str()));
        assert_eq!(extract_google_session(&headers, &raw), None);
    }
    for id in ["thread-one", "thread-two"] {
        raw["user_prompt_id"] = json!(format!("{id}########prompt"));
        assert_eq!(extract_google_session(&headers, &raw).as_deref(), Some(id));
    }
}

fn resolve_google_cache(raw: &Value) -> Result<Option<(&str, &str)>, &'static str> {
    let mut resolved = None;
    for container in [raw.get("request"), Some(raw)].into_iter().flatten() {
        for value in ["cachedContent", "cached_content"].into_iter().filter_map(|key| container.get(key)) {
            let cache = value.as_str().ok_or("cachedContent must be a string")?;
            let (project, _) = cache
                .strip_prefix("projects/")
                .and_then(|s| s.split_once('/'))
                .filter(|(project, resource)| !project.trim().is_empty() && !resource.trim().is_empty())
                .ok_or("cachedContent must name its owning project")?;
            if resolved.is_some_and(|(_, owner)| owner != project) {
                return Err("conflicting cachedContent projects");
            }
            resolved.get_or_insert((cache, project));
        }
    }
    Ok(resolved)
}

fn google_affinity_hint(raw: &Value, model: &str, cache: Option<&str>) -> Option<String> {
    // '#' is outside the explicit session ID namespace, including move-session.
    // Explicit Gemini cachedContent resources belong to a single project/account.
    if let Some(cached) = cache {
        let digest = hex::encode(&Sha256::digest(cached.as_bytes())[..12]);
        return Some(format!("#ag-cache-{digest}"));
    }
    // Derive a deterministic, privacy-safe conversation fingerprint from the stable
    // prefix (systemInstruction + first content turn) so multi-turn Antigravity
    // threads stay pinned to one account/project and preserve KV-cache hits.
    let container = raw.get("request").filter(|v| v.is_object()).unwrap_or(raw);
    let first_turn = container.get("contents").and_then(Value::as_array).and_then(|arr| arr.first())?;
    let mut hasher = Sha256::new();
    hasher.update(b"fleetix-ag-conv-v1\0");
    if let Some(scope) = model_cooldown_scope(Lane::Google, model) {
        hasher.update(scope.as_bytes());
    }
    hasher.update(b"\0");
    if let Some(system) = container.get("systemInstruction").or_else(|| container.get("system_instruction")) {
        hasher.update(serde_json::to_vec(system).unwrap_or_default());
    }
    hasher.update(b"\0");
    hasher.update(serde_json::to_vec(first_turn).unwrap_or_default());
    let digest = hex::encode(&hasher.finalize()[..12]);
    Some(format!("#ag-conv-{digest}"))
}

#[derive(Default)]
pub(crate) struct ProjectFields(Vec<(Range<usize>, String)>);

impl ProjectFields {
    /// Find project aliases in wire order and reject duplicate routing fields before
    /// consulting `Value`, which only keeps the last value for a repeated key.
    pub(crate) fn scan(body: &[u8]) -> Option<Self> {
        let mut entries = Vec::new();
        scan_unique_fields(body, ["project", "cloudaicompanionProject", "request", "cachedContent", "cached_content"], |name, span| {
            let value = &body[span.clone()];
            match name {
                "project" | "cloudaicompanionProject" => entries.push((span, serde_json::from_slice(value).ok()?)),
                "request" if value.starts_with(b"{") => {
                    scan_unique_fields(value, ["cachedContent", "cached_content"], |_, _| Some(()))?;
                }
                _ => {}
            }
            Some(())
        })?;
        Some(Self(entries))
    }
}

fn scan_unique_fields<const N: usize>(
    body: &[u8],
    fields: [&str; N],
    mut visit: impl FnMut(&str, Range<usize>) -> Option<()>,
) -> Option<()> {
    let mut seen = [false; N];
    let mut rest = body.trim_ascii_start().strip_prefix(b"{")?.trim_ascii_start();
    if rest.starts_with(b"}") {
        return Some(());
    }
    loop {
        let mut key = serde_json::Deserializer::from_slice(rest).into_iter::<String>();
        let name = key.next()?.ok()?;
        rest = rest[key.byte_offset()..].trim_ascii_start().strip_prefix(b":")?.trim_ascii_start();
        let start = body.len() - rest.len();
        let mut value = serde_json::Deserializer::from_slice(rest).into_iter::<serde::de::IgnoredAny>();
        value.next()?.ok()?;
        let len = value.byte_offset();
        if let Some(index) = fields.iter().position(|field| *field == name) {
            if std::mem::replace(&mut seen[index], true) {
                return None;
            }
            visit(&name, start..start + len)?;
        }
        rest = rest[len..].trim_ascii_start();
        if rest.starts_with(b"}") {
            return Some(());
        }
        rest = rest.strip_prefix(b",")?.trim_ascii_start();
    }
}

pub(crate) fn request_body(body: &Bytes, fields: &ProjectFields, account: &Account) -> Bytes {
    if let Some(project) = account.project.as_deref()
        && fields.0.iter().any(|(_, current)| current != project)
    {
        let encoded = Value::String(project.to_owned()).to_string();
        let mut out = body.to_vec();
        for (span, current) in fields.0.iter().rev() {
            if current != project {
                out.splice(span.clone(), encoded.bytes());
            }
        }
        return out.into();
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

fn header_id<'a>(headers: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| headers.get(*name)?.to_str().ok()).filter(|s| bounded_identifier(s))
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
            | "client-metadata"
            | "x-goog-api-client"
            | "x-goog-request-params"
            | "x-goog-api-format-version"
            | "x-client-version"
            | "x-client-request-id"
            | "x-session-id"
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

struct QuotaBody {
    chunks: [Option<Bytes>; 2],
    error: Option<reqwest::Error>,
    reset: Option<Cooldown>,
}

async fn read_google_quota_body(response: &mut reqwest::Response, deadline: tokio::time::Instant) -> QuotaBody {
    const LIMIT: usize = 64 * 1024;
    let mut prefix = Vec::new();
    let mut tail = None;
    let mut error = None;
    let mut boundary = JsonBoundary::default();
    let mut scanned = 0;
    let mut object = None;
    while prefix.len() < LIMIT {
        let chunk = match tokio::time::timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Err(err)) => {
                error = Some(err);
                break;
            }
            _ => break,
        };
        let take = chunk.len().min(LIMIT - prefix.len());
        prefix.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            tail = Some(chunk.slice(take..));
        }
        if object.is_none() {
            let trimmed = prefix[scanned..].trim_ascii_start();
            scanned = prefix.len() - trimmed.len();
            object = trimmed.first().map(|b| matches!(b, b'{' | b'['));
        }
        if object == Some(true) {
            if let Some(end) = boundary.scan(&prefix[scanned..]) {
                let reset = body_reset_hint(&prefix[..scanned + end], Utc::now());
                return QuotaBody { chunks: [Some(prefix.into()), tail], error, reset };
            }
            scanned = prefix.len();
        }
    }
    QuotaBody { chunks: [(!prefix.is_empty()).then(|| prefix.into()), tail], error, reset: None }
}

fn duration_seconds(mut raw: &str) -> Option<(f64, bool)> {
    if raw.is_empty() {
        return None;
    }
    let mut seconds = 0.0;
    let mut strict = true;
    while !raw.is_empty() {
        let split = raw.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (number, unit) = raw.split_at(split);
        let (factor, rest) = if let Some(rest) = unit.strip_prefix("ns") {
            (1e-9, rest)
        } else if let Some(rest) = unit.strip_prefix("us").or_else(|| unit.strip_prefix("µs")).or_else(|| unit.strip_prefix("μs")) {
            (1e-6, rest)
        } else if let Some(rest) = unit.strip_prefix("ms") {
            (0.001, rest)
        } else {
            let factor = match unit.as_bytes()[0] {
                b's' => 1.0,
                b'm' => 60.0,
                b'h' => 3600.0,
                b'd' => 86400.0,
                _ => return None,
            };
            (factor, &unit[1..])
        };
        seconds += number.parse::<f64>().ok()? * factor;
        strict &= !number.starts_with('.') && !number.ends_with('.');
        raw = rest;
    }
    Some((seconds, strict))
}

fn cooldown_after(now: DateTime<Utc>, seconds: i64) -> DateTime<Utc> {
    now + TimeDelta::seconds(seconds.clamp(1, MAX_COOLDOWN_SECS))
}

fn clamp_cooldown(now: DateTime<Utc>, at: DateTime<Utc>) -> Cooldown {
    let until = at.clamp(cooldown_after(now, 1), cooldown_after(now, MAX_COOLDOWN_SECS));
    Cooldown { until, verified_reset: until == at }
}

fn upstream_delay(now: DateTime<Utc>, seconds: f64) -> Cooldown {
    Cooldown { until: cooldown_after(now, seconds.ceil() as i64), verified_reset: (1.0..=MAX_COOLDOWN_SECS as f64).contains(&seconds) }
}

pub(crate) fn header_cooldown(lane: Lane, headers: &HeaderMap, now: DateTime<Utc>) -> Option<Cooldown> {
    let digits = |raw: &str| !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit());
    let verify = |mut cooldown: Cooldown, strict| {
        cooldown.verified_reset &= strict;
        cooldown
    };
    let clamp = |at| clamp_cooldown(now, at);
    let epoch =
        |n| clamp(DateTime::from_timestamp(n, 0).unwrap_or(if n < 0 { DateTime::<Utc>::MIN_UTC } else { DateTime::<Utc>::MAX_UTC }));
    let get = |name: &str| headers.get(name)?.to_str().ok();
    let single = |name: &str| headers.get_all(name).iter().count() == 1;
    // An unusable Retry-After may fall back for scheduling, but not for proof.
    let fallback_can_verify = !headers.contains_key("retry-after");
    // Preserve the legacy interpretation, but only attest known header/format pairs.
    let parse_reset = |name: &str, raw: &str| {
        let strict_source = fallback_can_verify && single(name);
        if let Ok(at) = raw.parse::<DateTime<Utc>>() {
            let known = matches!(
                name,
                "x-ratelimit-reset"
                    | "anthropic-ratelimit-requests-reset"
                    | "anthropic-ratelimit-tokens-reset"
                    | "anthropic-ratelimit-input-tokens-reset"
                    | "anthropic-ratelimit-output-tokens-reset"
            );
            let strict = matches!(raw.as_bytes().get(10), Some(&b'T' | &b't')) && DateTime::parse_from_rfc3339(raw).is_ok();
            return Some(verify(clamp(at), strict_source && known && strict));
        }
        if let Ok(n) = raw.parse::<i64>() {
            let is_epoch = n > 1_000_000_000;
            let known = name == "x-ratelimit-reset"
                || (is_epoch
                    && matches!(
                        name,
                        "anthropic-ratelimit-unified-reset"
                            | "anthropic-ratelimit-unified-5h-reset"
                            | "anthropic-ratelimit-unified-7d-reset"
                    ));
            let cooldown = if is_epoch { epoch(n) } else { upstream_delay(now, n as f64) };
            return Some(verify(cooldown, strict_source && known && digits(raw)));
        }
        let known = matches!(name, "x-ratelimit-reset" | "x-ratelimit-reset-requests" | "x-ratelimit-reset-tokens");
        duration_seconds(raw).map(|(n, strict)| verify(upstream_delay(now, n), strict_source && known && strict))
    };
    if let Some(at) = get("retry-after").and_then(|raw| {
        raw.parse::<i64>().ok().filter(|n| *n >= 0).map(|n| verify(upstream_delay(now, n as f64), digits(raw))).or_else(|| {
            let at = DateTime::parse_from_rfc2822(raw).ok()?.with_timezone(&Utc);
            let strict = raw == at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
            Some(verify(clamp(at), strict))
        })
    }) {
        return Some(verify(at, single("retry-after")));
    }
    if lane == Lane::Codex {
        let windows = ["x-codex-primary", "x-codex-secondary"].map(|prefix| {
            let percent = format!("{prefix}-used-percent");
            let name = format!("{prefix}-reset-at");
            let exhausted = get(&percent).and_then(|s| s.parse::<f64>().ok()).is_some_and(|n| n >= 100.0);
            let reset = get(&name).and_then(|s| {
                Some(verify(
                    epoch(s.parse::<i64>().ok()?),
                    fallback_can_verify && single(&name) && (!exhausted || single(&percent)) && digits(s),
                ))
            });
            (exhausted, reset)
        });
        if windows.iter().any(|(exhausted, _)| *exhausted) {
            return windows.iter().filter_map(|(exhausted, at)| exhausted.then_some(*at).flatten()).reduce(Cooldown::merge);
        }
        if let Some(at) = windows.into_iter().find_map(|(_, at)| at) {
            return Some(at);
        }
    }
    if lane == Lane::Claude {
        let mut exhausted = headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.as_str();
                if !name.starts_with("anthropic-ratelimit-") {
                    return None;
                }
                let window = match value.to_str().ok()? {
                    "rejected" => name.strip_suffix("-status"),
                    "0" => name.strip_suffix("-remaining"),
                    _ => None,
                }?;
                Some((window, single(name)))
            })
            .peekable();
        if exhausted.peek().is_some() {
            return exhausted
                .filter_map(|(window, unique)| {
                    let name = format!("{window}-reset");
                    parse_reset(&name, get(&name)?).map(|at| verify(at, unique))
                })
                .reduce(Cooldown::merge);
        }
    }
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str();
            ((name.starts_with("anthropic-ratelimit-") || name.starts_with("x-ratelimit-"))
                && (name.ends_with("-reset") || name.contains("-reset-")))
            .then(|| parse_reset(name, value.to_str().ok()?))
            .flatten()
        })
        .reduce(Cooldown::merge)
}

fn body_reset_hint(body: &[u8], now: DateTime<Utc>) -> Option<Cooldown> {
    let raw: Value = serde_json::from_slice(body).ok()?;
    google_reset_hint(&raw, now)
}

fn google_detail_kind(detail: &Value) -> Option<&str> {
    detail["@type"].as_str()?.rsplit('/').next()
}

fn google_reset_delay(value: &Value, now: DateTime<Utc>) -> Option<Cooldown> {
    value.as_str().and_then(duration_seconds).map(|(secs, _)| upstream_delay(now, secs))
}

fn google_reset_timestamp(value: &Value, now: DateTime<Utc>) -> Option<Cooldown> {
    value.as_str()?.parse::<DateTime<Utc>>().ok().map(|at| clamp_cooldown(now, at))
}

fn google_reset_hint(raw: &Value, now: DateTime<Utc>) -> Option<Cooldown> {
    if let Value::Array(items) = raw {
        return items.iter().filter_map(|item| google_reset_hint(item, now)).reduce(Cooldown::merge);
    }
    raw.pointer("/error/details")
        .and_then(Value::as_array)?
        .iter()
        .flat_map(|detail| match google_detail_kind(detail) {
            Some("google.rpc.RetryInfo" | "RetryInfo") => [google_reset_delay(&detail["retryDelay"], now), None],
            Some("google.rpc.ErrorInfo" | "ErrorInfo") => [
                google_reset_delay(&detail["metadata"]["quotaResetDelay"], now),
                google_reset_timestamp(&detail["metadata"]["quotaResetTimeStamp"], now),
            ],
            _ => [None, None],
        })
        .flatten()
        .reduce(Cooldown::merge)
}

fn merge_quota_hints(headers: &HeaderMap, header: Option<Cooldown>, body: Option<Cooldown>) -> Option<Cooldown> {
    let mut cooldown = header.into_iter().chain(body).reduce(Cooldown::merge)?;
    // An unusable Retry-After must not be promoted by a later body hint.
    cooldown.verified_reset &= !headers.contains_key(header::RETRY_AFTER) || header.is_some_and(|hint| hint.verified_reset);
    Some(cooldown)
}

fn quota_hint(lane: Lane, headers: &HeaderMap, body: Option<Cooldown>, now: DateTime<Utc>) -> Option<Cooldown> {
    merge_quota_hints(headers, header_cooldown(lane, headers, now), body)
}

fn quota_deadline(hint: Option<Cooldown>, fallback: u64, now: DateTime<Utc>) -> Cooldown {
    hint.unwrap_or_else(|| Cooldown { until: cooldown_after(now, fallback.min(MAX_COOLDOWN_SECS as u64) as i64), verified_reset: false })
}

#[cfg(test)]
pub(crate) fn body_cooldown(lane: Lane, body: &[u8], fallback: u64, now: DateTime<Utc>) -> Cooldown {
    quota_deadline((lane == Lane::Google).then(|| body_reset_hint(body, now)).flatten(), fallback, now)
}

#[cfg(test)]
pub(crate) fn cooldown(lane: Lane, headers: &HeaderMap, body: &[u8], fallback: u64) -> DateTime<Utc> {
    let now = Utc::now();
    let body = (lane == Lane::Google).then(|| body_reset_hint(body, now)).flatten();
    quota_deadline(quota_hint(lane, headers, body, now), fallback, now).until
}

#[derive(Clone, Copy)]
enum Backoff {
    Auth(DateTime<Utc>),
    Quota(Cooldown),
}

fn auth_backoff(fallback: u64) -> Backoff {
    Backoff::Auth(cooldown_after(Utc::now(), fallback.clamp(1, 60) as i64))
}

async fn result(proxy: &Proxy, account: &Account, model: &str, cooldown: Option<Backoff>, success: bool) {
    let mut state = proxy.state.write().await;
    let Some(current) = state.pool.accounts.get_mut(&account.id).filter(|a| a.same_credential(account)) else { return };
    let now = Utc::now();
    current.last_used = Some(now);
    if success {
        current.success = current.success.saturating_add(1);
    } else {
        current.failure = current.failure.saturating_add(1);
    }
    match cooldown {
        Some(Backoff::Auth(until)) => {
            current.auth_cooldown = current.auth_cooldown.max(Some(until));
        }
        Some(Backoff::Quota(until)) => {
            apply_quota(current, model, until, now);
        }
        None => {}
    }
}

async fn extend_quota(proxy: &Proxy, account: &Account, model: &str, until: Option<Cooldown>) {
    let Some(until) = until else { return };
    let mut state = proxy.state.write().await;
    if let Some(current) = state.pool.accounts.get_mut(&account.id).filter(|a| a.same_credential(account)) {
        apply_quota(current, model, until, Utc::now());
    }
}

fn apply_quota(account: &mut Account, model: &str, until: Cooldown, now: DateTime<Utc>) {
    if let Some(scope) = model_cooldown_scope(account.lane, model) {
        account.scoped_cooldowns.retain(|_, c| c.until > now);
        let entry = account.scoped_cooldowns.entry(scope).or_insert(until);
        *entry = entry.merge(until);
    } else {
        account.cooldown = Some(account.cooldown.map_or(until, |old| old.merge(until)));
    }
}

fn millis(duration: Duration) -> i64 {
    duration.as_millis().min(i64::MAX as u128) as i64
}

#[derive(Clone, Copy, Default)]
struct Usage {
    input: i64,
    explicit_output: i64,
    observed_total: Option<i64>,
    cached: i64,
    written: i64,
    thinking: i64,
}

impl Usage {
    fn observe(&mut self, raw: &Value, lane: Lane) {
        if let Value::Array(items) = raw {
            for item in items {
                self.observe(item, lane);
            }
            return;
        }
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
        let cached = get(&[
            "cache_read_input_tokens",
            "/input_tokens_details/cached_tokens",
            "/prompt_tokens_details/cached_tokens",
            "cachedContentTokenCount",
            "cached_tokens",
        ])
        .unwrap_or_else(|| {
            usage
                .get("cacheTokensDetails")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("tokenCount").and_then(Value::as_i64))
                .filter(|n| *n > 0)
                .fold(0i64, i64::saturating_add)
        });
        self.cached = self.cached.max(cached);
        update(&mut self.written, &["cache_creation_input_tokens", "cacheCreationTokenCount", "cache_creation_tokens"]);
        update(&mut self.input, &["input_tokens", "prompt_tokens", "promptTokenCount"]);
        update(&mut self.explicit_output, &["output_tokens", "completion_tokens", "candidatesTokenCount"]);
        update(
            &mut self.thinking,
            &["/output_tokens_details/reasoning_tokens", "/completion_tokens_details/reasoning_tokens", "thoughtsTokenCount"],
        );
        if lane == Lane::Google
            && let Some(total) = get(&["totalTokenCount"])
        {
            self.observed_total = Some(total);
        }
    }

    fn output(self, lane: Lane) -> i64 {
        let input = self.total(lane);
        let inferred = self.observed_total.filter(|_| input > 0).map_or(0, |total| total.saturating_sub(input));
        self.explicit_output.max(inferred)
    }

    fn total(self, lane: Lane) -> i64 {
        if lane == Lane::Claude {
            self.input.saturating_add(self.cached).saturating_add(self.written)
        } else {
            self.input.max(self.cached.saturating_add(self.written))
        }
    }
}

#[cfg(test)]
#[test]
fn google_usage_observes_array_and_later_cache_details_without_counting_prompt_as_output() {
    let mut usage = Usage::default();
    usage.observe(&json!({"usageMetadata":{"totalTokenCount":100}}), Lane::Google);
    assert_eq!(usage.output(Lane::Google), 0);
    usage.observe(
        &json!([
            {"usageMetadata":{"cacheTokensDetails":[{"tokenCount":10}]}},
            {"response":{"usageMetadata":{"cacheTokensDetails":[{"tokenCount":30},{"tokenCount":20}],
                "cacheCreationTokenCount":5,"totalTokenCount":62}}}
        ]),
        Lane::Google,
    );
    assert_eq!((usage.cached, usage.total(Lane::Google), usage.output(Lane::Google)), (50, 55, 7));
    usage.observe(&json!({"usageMetadata":{"cacheTokensDetails":[{"tokenCount":60}]}}), Lane::Google);
    assert_eq!((usage.cached, usage.output(Lane::Google)), (60, 0));
}

fn is_sse(headers: &HeaderMap) -> bool {
    headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/event-stream"))
}

fn inspect_google(headers: &HeaderMap) -> bool {
    headers.get_all(header::CONTENT_ENCODING).iter().all(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(',').all(|coding| {
                let coding = coding.trim();
                coding.is_empty() || coding.eq_ignore_ascii_case("identity")
            })
        })
    })
}

#[derive(Clone, Copy)]
struct GoogleError {
    status: StatusCode,
    reset: Option<Cooldown>,
}

impl GoogleError {
    fn from_frame(raw: &Value) -> Option<(Self, &Value)> {
        if let Value::Array(items) = raw {
            return items.iter().find_map(Self::from_frame);
        }
        let error = raw.get("error")?.as_object()?;
        let code = error.get("code").and_then(Value::as_u64);
        let rpc = error.get("status").and_then(Value::as_str);
        // google.rpc.Code values (in code order) also arrive without an HTTP code.
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
        let http = match (code, rpc) {
            (Some(8 | 429), _) | (_, Some("RESOURCE_EXHAUSTED")) => Some(429),
            _ => code
                .and_then(|n| u16::try_from(n).ok())
                .and_then(|n| if n >= 400 { Some(n) } else { RPC.get(usize::from(n)).map(|(_, http)| *http) })
                .or_else(|| RPC.iter().find(|(name, _)| Some(*name) == rpc).map(|(_, http)| *http)),
        };
        let status = http
            .and_then(|n| StatusCode::from_u16(n).ok())
            .filter(|status| status.is_client_error() || status.is_server_error())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        Some((Self { status, reset: google_reset_hint(raw, Utc::now()) }, raw))
    }

    fn backoff(self, headers: &HeaderMap, fallback: u64, now: DateTime<Utc>) -> Option<Backoff> {
        match self.status {
            StatusCode::TOO_MANY_REQUESTS => {
                Some(Backoff::Quota(quota_deadline(quota_hint(Lane::Google, headers, self.reset, now), fallback, now)))
            }
            StatusCode::UNAUTHORIZED => Some(auth_backoff(fallback)),
            _ => None,
        }
    }

    fn outcome(self) -> &'static str {
        if self.status == StatusCode::TOO_MANY_REQUESTS { "rate_limited" } else { "http_error" }
    }
}

fn google_has_output(raw: &Value) -> bool {
    if let Value::Array(items) = raw {
        return items.iter().any(google_has_output);
    }
    raw.get("candidates").or_else(|| raw.pointer("/response/candidates")).and_then(Value::as_array).is_some_and(|items| {
        items.iter().any(|candidate| candidate.pointer("/content/parts").and_then(Value::as_array).is_some_and(|parts| !parts.is_empty()))
    })
}

const GOOGLE_FRAME_LIMIT: usize = 1024 * 1024;

/// Track the end of a JSON value without buffering or decoding it.
#[derive(Default)]
struct JsonBoundary {
    depth: usize,
    string: bool,
    escaped: bool,
}

impl JsonBoundary {
    fn scan(&mut self, bytes: &[u8]) -> Option<usize> {
        for (index, byte) in bytes.iter().copied().enumerate() {
            if self.string {
                match byte {
                    _ if std::mem::take(&mut self.escaped) => {}
                    b'\\' => self.escaped = true,
                    b'"' => {
                        self.string = false;
                        if self.depth == 0 {
                            return Some(index + 1);
                        }
                    }
                    _ => {}
                }
            } else {
                match byte {
                    b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t' if self.depth == 0 => return Some(index),
                    b'"' => self.string = true,
                    b'{' | b'[' => self.depth += 1,
                    b'}' | b']' => {
                        self.depth -= 1;
                        if self.depth == 0 {
                            return Some(index + 1);
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }
}

enum GoogleFormat {
    Sse,
    SseDiscard,
    Json,
    Array,
    ArrayDiscard(JsonBoundary),
    Opaque,
}

struct GoogleFrames {
    format: GoogleFormat,
    pending: Bytes,
    sse_after_cr: bool,
}

struct GoogleFrame {
    bytes: Bytes,
    json: Option<(Range<usize>, Value)>,
}

fn google_json_value(bytes: &[u8], array: bool) -> &[u8] {
    let trimmed = bytes.trim_ascii_start();
    if array { trimmed.strip_prefix(b",").unwrap_or(trimmed).trim_ascii_start() } else { trimmed }
}

impl GoogleFrames {
    fn new(sse: bool) -> Self {
        Self { format: if sse { GoogleFormat::Sse } else { GoogleFormat::Json }, pending: Bytes::new(), sse_after_cr: false }
    }

    fn push(&mut self, bytes: Bytes) {
        if self.pending.is_empty() {
            self.pending = bytes;
        } else {
            let mut pending = Vec::with_capacity(self.pending.len() + bytes.len());
            pending.extend_from_slice(&self.pending);
            pending.extend_from_slice(&bytes);
            self.pending = pending.into();
        }
    }

    fn next(&mut self, eof: bool) -> Option<GoogleFrame> {
        // ponytail: Cloud Code emits one JSON value per data line or array item.
        // Inspect at most 1 MiB per value; larger items pass through unchanged.
        let bytes = &self.pending[..self.pending.len().min(GOOGLE_FRAME_LIMIT)];
        if bytes.is_empty() {
            return None;
        }
        let full = self.pending.len() >= GOOGLE_FRAME_LIMIT;
        let mut json = None;
        let len = match &mut self.format {
            GoogleFormat::Sse | GoogleFormat::SseDiscard if std::mem::take(&mut self.sse_after_cr) && bytes[0] == b'\n' => 1,
            GoogleFormat::Sse | GoogleFormat::SseDiscard => {
                let line_end = bytes.iter().position(|b| matches!(b, b'\r' | b'\n'));
                let end = if let Some(i) = line_end {
                    let cr = bytes[i] == b'\r';
                    // Emit CR immediately; a following LF can be consumed as framing on the next chunk.
                    self.sse_after_cr = cr && i + 1 == bytes.len();
                    i + 1 + usize::from(cr && bytes.get(i + 1) == Some(&b'\n'))
                } else {
                    (eof || full).then_some(bytes.len())?
                };
                if matches!(self.format, GoogleFormat::Sse)
                    && let Some((start, payload)) = bytes[..end].strip_prefix(b"data:").map(|data| {
                        let data = data.trim_ascii_start();
                        (end - data.len(), data.trim_ascii_end())
                    })
                    && let Ok(raw) = serde_json::from_slice(payload)
                {
                    json = Some((start..start + payload.len(), raw));
                }
                self.format = if line_end.is_some() { GoogleFormat::Sse } else { GoogleFormat::SseDiscard };
                end
            }
            GoogleFormat::Json | GoogleFormat::Array => {
                let array = matches!(self.format, GoogleFormat::Array);
                let value = google_json_value(bytes, array);
                let start = bytes.len() - value.len();
                match value.first() {
                    Some(b'[') if !array => {
                        self.format = GoogleFormat::Array;
                        start + 1
                    }
                    Some(first) if matches!(first, b'{' | b'[') || (array && *first != b']') => {
                        let mut values = serde_json::Deserializer::from_slice(value).into_iter::<Value>();
                        match values.next()? {
                            Ok(raw) => {
                                let end = start + values.byte_offset();
                                if raw.is_number() && end == bytes.len() && !eof && !full {
                                    return None;
                                }
                                json = Some((start..end, raw));
                                if !array {
                                    self.format = GoogleFormat::Opaque;
                                }
                                end
                            }
                            Err(err) if err.is_eof() && !eof && !full => return None,
                            Err(_) if array => {
                                let mut boundary = JsonBoundary::default();
                                if let Some(end) = boundary.scan(value) {
                                    start + end.max(1)
                                } else {
                                    self.format = GoogleFormat::ArrayDiscard(boundary);
                                    bytes.len()
                                }
                            }
                            Err(_) => {
                                self.format = GoogleFormat::Opaque;
                                self.pending.len()
                            }
                        }
                    }
                    None if !eof && !full => return None,
                    None if array => bytes.len(),
                    _ => {
                        self.format = GoogleFormat::Opaque;
                        self.pending.len()
                    }
                }
            }
            GoogleFormat::ArrayDiscard(boundary) => {
                if let Some(end) = boundary.scan(bytes) {
                    self.format = GoogleFormat::Array;
                    if end == 0 {
                        return self.next(eof);
                    }
                    end
                } else {
                    bytes.len()
                }
            }
            GoogleFormat::Opaque => self.pending.len(),
        };
        Some(GoogleFrame { bytes: self.pending.split_to(len), json })
    }

    fn flush_framing(&mut self) -> Option<Bytes> {
        // Keep a partial value intact so a late quota error can still be rewritten.
        let array = match self.format {
            GoogleFormat::Json => false,
            GoogleFormat::Array => true,
            _ => return None,
        };
        let len = self.pending.len() - google_json_value(&self.pending, array).len();
        (len > 0).then(|| self.pending.split_to(len))
    }
}

#[cfg(test)]
#[test]
fn google_array_recovers_after_an_item_exceeds_the_decode_depth() {
    let nested = format!("{}null{}", "[".repeat(150), "]".repeat(150));
    let body = Bytes::from(format!("[{nested},{{\"error\":{{\"code\":429}}}}]"));
    for split in [151, body.len()] {
        let mut frames = GoogleFrames::new(false);
        let mut forwarded = Vec::new();
        let mut status = None;
        for (chunk, eof) in [(body.slice(..split), false), (body.slice(split..), true)] {
            frames.push(chunk);
            while let Some(frame) = frames.next(eof) {
                forwarded.extend_from_slice(&frame.bytes);
                if let Some((_, raw)) = frame.json {
                    status = GoogleError::from_frame(&raw).map(|(error, _)| error.status);
                }
            }
        }
        assert_eq!(status, Some(StatusCode::TOO_MANY_REQUESTS));
        assert_eq!(forwarded, body);
    }
}

#[derive(Default)]
struct GooglePrelude {
    first: [Option<Bytes>; 2],
    error: Option<(GoogleError, Value)>,
    usage: Usage,
    stream_error: Option<reqwest::Error>,
    flush_prefix: bool,
}

async fn google_first(response: &mut reqwest::Response, first: Bytes, sse: bool) -> GooglePrelude {
    let mut frames = GoogleFrames::new(sse);
    let mut received = first.len();
    frames.push(first);
    let mut prefix = Vec::new();
    let mut eof = false;
    let mut prelude = GooglePrelude::default();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        if received > GOOGLE_FRAME_LIMIT || tokio::time::Instant::now() >= deadline {
            prelude.flush_prefix = true;
            break;
        }
        if let Some(frame) = frames.next(eof) {
            prefix.extend_from_slice(&frame.bytes);
            if let Some((_, raw)) = &frame.json {
                prelude.usage.observe(raw, Lane::Google);
                if google_has_output(raw) {
                    break;
                }
                if let Some((error, raw)) = GoogleError::from_frame(raw) {
                    prelude.error = Some((error, raw.clone()));
                    break;
                }
            }
            if matches!(frames.format, GoogleFormat::Opaque) {
                break;
            }
        } else if eof {
            break;
        } else if received == GOOGLE_FRAME_LIMIT {
            prelude.flush_prefix = true;
            break;
        } else {
            match tokio::time::timeout_at(deadline, response.chunk()).await {
                Ok(Ok(Some(bytes))) => {
                    received = received.saturating_add(bytes.len());
                    frames.push(bytes);
                }
                Ok(Ok(None)) => eof = true,
                Ok(Err(err)) => {
                    prelude.stream_error = Some(err);
                    break;
                }
                Err(_) => {
                    prelude.flush_prefix = true;
                    break;
                }
            }
        }
    }
    prelude.first = [(!prefix.is_empty()).then(|| prefix.into()), (!frames.pending.is_empty()).then_some(frames.pending)];
    prelude
}

struct Wire {
    proxy: Arc<Proxy>,
    guard: RecordOnDrop,
    account: Account,
    status: StatusCode,
    started: Instant,
    upstream: reqwest::Response,
    first: [Option<Bytes>; 2],
    stream_error: Option<reqwest::Error>,
    flush_google_prefix: bool,
    upstream_done: bool,
    sse: bool,
    google: Option<GoogleFrames>,
    fallback: u64,
    usage: Usage,
    usage_buf: Vec<u8>,
    discard_line: bool,
    capture_buf: Vec<u8>,
    response_size: usize,
}

impl Wire {
    async fn next_frame(&mut self) -> Result<Option<GoogleFrame>, reqwest::Error> {
        loop {
            let end = (self.upstream_done || self.stream_error.is_some()) && self.first.iter().all(Option::is_none);
            if let Some(frame) = self.google.as_mut().and_then(|frames| frames.next(end)) {
                return Ok(Some(frame));
            }
            let bytes = if let Some(bytes) = self.first[0].take().or_else(|| self.first[1].take()) {
                bytes
            } else if std::mem::take(&mut self.flush_google_prefix)
                && let Some(bytes) = self.google.as_mut().and_then(GoogleFrames::flush_framing)
            {
                return Ok(Some(GoogleFrame { bytes, json: None }));
            } else if let Some(err) = self.stream_error.take() {
                // Let the downstream dispatcher flush the buffered prefix before it aborts the body.
                tokio::task::yield_now().await;
                return Err(err);
            } else if self.upstream_done {
                return Ok(None);
            } else {
                match self.upstream.chunk().await {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        self.upstream_done = true;
                        continue;
                    }
                    Err(err) => {
                        self.stream_error = Some(err);
                        continue;
                    }
                }
            };
            if let Some(frames) = &mut self.google {
                frames.push(bytes);
            } else {
                return Ok(Some(GoogleFrame { bytes, json: None }));
            }
        }
    }

    async fn google_frame(&mut self, mut frame: GoogleFrame) -> Bytes {
        if let Some((span, mut raw)) = frame.json.take() {
            self.usage.observe(&raw, Lane::Google);
            if let Some((error, _)) = GoogleError::from_frame(&raw)
                && let Some(exchange) = &mut self.guard.0
            {
                let cooldown = error.backoff(self.upstream.headers(), self.fallback, Utc::now());
                let first_error = self.guard.1.is_none();
                if first_error {
                    result(&self.proxy, &self.account, &exchange.model, cooldown, false).await;
                    if let Some(last) = exchange.attempts.last_mut() {
                        last.status_code = Some(i32::from(error.status.as_u16()));
                        last.outcome = error.outcome().into();
                    }
                    self.guard.1 = Some((error, self.usage, millis(self.started.elapsed())));
                } else if let Some(Backoff::Quota(until)) = cooldown {
                    extend_quota(&self.proxy, &self.account, &exchange.model, Some(until)).await;
                }
                if matches!(cooldown, Some(Backoff::Quota(_))) {
                    if first_error {
                        self.proxy.schedule_usage_refresh(self.account.id.clone()).await;
                    }
                    normalize_google_quota(&mut raw, &exchange.model, Utc::now());
                    let mut bytes = frame.bytes.to_vec();
                    bytes.splice(span, raw.to_string().bytes());
                    return bytes.into();
                }
            }
        }
        frame.bytes
    }

    async fn next(mut self) -> Result<Option<(Bytes, Self)>, reqwest::Error> {
        let chunk = match self.next_frame().await {
            Ok(Some(frame)) => Ok(Some(self.google_frame(frame).await)),
            Ok(None) => Ok(None),
            Err(err) => Err(err),
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
            if self.google.is_none() && self.sse {
                for chunk in data.split_inclusive(|b| *b == b'\n') {
                    if chunk.len() + self.usage_buf.len() > 1024 * 1024 {
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
            } else if self.google.is_none() && self.response_size <= 1024 * 1024 {
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
                if self.status.is_success() && self.guard.1.is_none() {
                    result(&self.proxy, &self.account, "", None, done.is_ok()).await;
                }
                if let Some(mut exchange) = self.guard.0.take() {
                    let outcome = if let Some((error, _, _)) = self.guard.1 {
                        error.outcome()
                    } else if done.is_err() {
                        "stream_error"
                    } else if self.status.is_success() {
                        "success"
                    } else if self.account.lane == Lane::Google && self.status == StatusCode::TOO_MANY_REQUESTS {
                        "rate_limited"
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
                            Some(self.guard.1.map_or(self.status, |(error, _, _)| error.status)),
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

struct RecordOnDrop(Option<Exchange>, Option<(GoogleError, Usage, i64)>);
impl Drop for RecordOnDrop {
    fn drop(&mut self) {
        if let Some(mut exchange) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let failure = self.1;
            runtime.spawn(async move {
                if let Some((error, usage, latency)) = failure {
                    exchange.finish(Some(error.status), error.outcome(), usage, Some(latency), None).await;
                    return;
                }
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
    effort: Option<String>,
    purpose: ActivityPurpose,
    session: Option<String>,
    affinity_key: Option<String>,
    parent: Option<String>,
    attempts: Vec<ActivityAttemptIn>,
    capture: Option<Capture>,
}

impl Exchange {
    fn new(
        proxy: Arc<Proxy>,
        lane: Lane,
        model: &str,
        effort: Option<String>,
        session: Option<String>,
        parent: Option<String>,
        purpose: ActivityPurpose,
    ) -> Self {
        Self {
            proxy,
            id: Uuid::new_v4().to_string(),
            started: Instant::now(),
            occurred: Utc::now(),
            lane,
            model: model.into(),
            effort,
            purpose,
            affinity_key: session.clone(),
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
            effort: self.effort.clone(),
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
        if let Some(last) = self.attempts.last_mut().filter(|last| last.finished_at.is_none()) {
            last.finished_at = Some(Utc::now());
            last.latency_ms = latency.or(last.latency_ms);
            last.input_tokens = usage.total(self.lane);
            last.output_tokens = usage.output(self.lane);
            last.cache_read_tokens = usage.cached;
            last.cache_write_tokens = usage.written;
            last.thinking_tokens = usage.thinking;
        }
    }

    async fn rate_limited(self, quota: PoolQuota, usage: Usage, latency: Option<i64>) -> Response {
        let response = pool_limit(self.lane, &self.model, quota);
        self.finish(Some(StatusCode::TOO_MANY_REQUESTS), "rate_limited", usage, latency, None).await;
        response
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
            effort: self.effort,
            outcome: outcome.into(),
            status_code: status.map(|s| i32::from(s.as_u16())),
            error_message: None,
            latency_ms: Some(millis(self.started.elapsed())),
            ttft_ms: last.and_then(|a| a.ttft_ms),
            input_tokens: usage.total(self.lane),
            output_tokens: usage.output(self.lane),
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

#[cfg(test)]
#[tokio::test]
async fn google_finish_without_an_alternate_keeps_completed_attempt_usage() {
    use crate::tests::{Scratch, fixture, serve};

    let dir = Scratch::new();
    let mock = serve(Router::new()).await;
    let proxy = fixture(&dir, &mock);
    let account = crate::pool::parse_credentials(
        "google-a.json",
        &json!({"type":"google","access_token":"fixture","email":"fixture@example.test"}),
        false,
    )
    .unwrap()
    .pop()
    .unwrap();
    let mut exchange = Exchange::new(proxy.clone(), Lane::Google, "gemini", None, None, None, ActivityPurpose::Ordinary);
    exchange.attempt(&account, None, Utc::now(), Instant::now(), Some(StatusCode::UNAUTHORIZED), "http_error");
    exchange.complete_attempt(Usage { input: 100, cached: 20, ..Usage::default() }, Some(42));
    let finished_at = exchange.attempts[0].finished_at.unwrap();
    exchange.finish(Some(StatusCode::SERVICE_UNAVAILABLE), "unavailable", Usage::default(), None, None).await;
    let state = proxy.state.read().await;
    let attempt = &state.activity.back().unwrap().attempts[0];
    assert_eq!((attempt.input_tokens, attempt.cache_read_tokens, attempt.latency_ms), (100, 20, Some(42)));
    assert_eq!(attempt.finished_at, Some(finished_at));
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
        "usageMetadata",
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "promptTokenCount",
        "candidatesTokenCount",
        "cachedContentTokenCount",
        "cacheCreationTokenCount",
        "thoughtsTokenCount",
        "totalTokenCount",
        "cacheTokensDetails",
        "tokenCount",
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
                    .map(|(k, v)| (k.clone(), redact(v, k.ends_with("_tokens") || k.ends_with("TokenCount") || k == "tokenCount")))
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
