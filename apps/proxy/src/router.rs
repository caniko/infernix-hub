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
use contracts::providers::{
    AccountError, AccountErrorKind, AccountErrorSource, ActivityAttemptIn, ActivityExchangeIn, ActivityPurpose, ActivitySource,
    CaptureState, TraceEdgeKind,
};
use futures_util::stream;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    ACTIVITY_CAP, CAPTURE_BYTES, Proxy,
    config::matches_secret,
    error_classify::{ERROR_BODY_LIMIT, account_secrets, classify_sse_error, classify_upstream_http_error, local_error, safe_label},
    openai_account,
    pool::{Account, Cooldown, Lane, MAX_COOLDOWN_SECS, Pool, QuotaReader, bounded_identifier, model_cooldown_scope, text},
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
    until: Option<DateTime<Utc>>,
    reader: Option<tokio::task::JoinHandle<Option<DateTime<Utc>>>>,
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
    ReadBody(usize, tokio::task::JoinHandle<Option<DateTime<Utc>>>),
}

#[cfg(test)]
tokio::task_local! {
    pub(super) static AFTER_FAILOVER_PREVIEW: fn(&mut crate::pool::Pool);
}

pub(super) async fn forward(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
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
        let supported = |account: &Account| pool.supports(account, lane, model, required_project) && !account.quarantined(now);
        let reset = pool
            .accounts
            .values()
            .filter(|account| supported(account))
            .filter_map(|account| {
                live_deadline(account).or_else(|| {
                    retained
                        .iter()
                        .find(|quota| quota.account.id == account.id && account.same_credential(&quota.account))
                        .and_then(|quota| quota.until)
                })
            })
            .min()?;
        let verified_reset = pool.verified_google_reset_at(model, required_project, now);
        let pending = retained.iter_mut().enumerate().find_map(|(index, quota)| {
            let current = pool.accounts.get(&quota.account.id).filter(|a| a.same_credential(&quota.account) && supported(a))?;
            if live_deadline(current).or(quota.until)? > reset.max(verified_reset.unwrap_or(reset)) {
                return None;
            }
            Some(QuotaSnapshot::ReadBody(index, quota.reader.take()?))
        });
        pending.or(Some(QuotaSnapshot::Ready(PoolQuota { reset, verified_reset, now })))
    };
    let pick = |pool: &mut Pool, exchange: &Exchange, tried: &[String], now| {
        pool.pick(lane, (exchange.affinity_key.as_deref(), exchange.parent.as_deref()), model, required_project, tried, now)
    };
    let pick_or_quota = async |exchange: &Exchange, tried: &[String], retained: &mut [RetainedQuota]| {
        loop {
            let snapshot = {
                let mut state = proxy.state.write().await;
                let now = Utc::now();
                if let Some(account) = pick(&mut state.pool, exchange, tried, now) {
                    return (Some(account), None, None);
                }
                match quota_snapshot(&state.pool, now, retained) {
                    Some(snapshot) => snapshot,
                    None => return (None, None, unavailable_pool(&state.pool, lane, model, required_project, now)),
                }
            };
            match snapshot {
                QuotaSnapshot::ReadBody(index, reader) => {
                    if let Ok(until) = reader.await {
                        retained[index].until = until;
                    }
                    // Recheck both eligibility and quota after the body settles.
                }
                QuotaSnapshot::Ready(quota) => return (None, Some(quota), None),
            }
        }
    };
    let mut next_account = None;
    loop {
        let (account, quota, unavailable) = match next_account.take() {
            Some(account) => (Some(account), None, None),
            None => pick_or_quota(&exchange, &tried, &mut retained_quota).await,
        };
        let Some(mut account) = account else {
            if let Some(quota) = quota.or_else(|| last_decoded_quota_reset.map(PoolQuota::unverified)) {
                return exchange.rate_limited(quota, Usage::default(), None).await;
            }
            if let Some(response) = unavailable {
                exchange.finish(Some(StatusCode::SERVICE_UNAVAILABLE), "unavailable", Usage::default(), None, None).await;
                return response;
            }
            exchange.finish(Some(last_error), "unavailable", Usage::default(), None, None).await;
            return lane_error(lane, last_error, "no eligible upstream account");
        };
        if !tried.is_empty() {
            proxy.log("warn", &format!("{} request {} failed over to {}", lane.name(), exchange.id, safe_label(&account))).await;
        }
        tried.push(account.id.clone());
        let began = Instant::now();
        let started = Utc::now();
        let timeout = Duration::from_secs(policy.first_byte_timeout_secs.clamp(1, 600));
        let deadline = tokio::time::Instant::now() + timeout;
        let reload_on_401 = !account.api_key && matches!(lane, Lane::Claude | Lane::Codex);
        let incoming = async {
            let upstream_error = || inference_error("upstream request failed");
            let resolved = proxy.token_account(&account, false).await.map_err(|error| *error)?;
            if required_project.is_some_and(|project| resolved.project.as_deref() != Some(project)) {
                return Err(upstream_error());
            }
            account = resolved;
            let url = upstream(&proxy, &account, &parts.uri).ok_or_else(upstream_error)?;
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
            let mut res = build(&account).send().await.map_err(|_| upstream_error())?;
            let mut refresh_error = None;
            if res.status() == StatusCode::UNAUTHORIZED
                && (reload_on_401 || (lane == Lane::Google && !account.api_key && account.refresh.is_some()))
            {
                match proxy.token_account(&account, true).await {
                    Ok(fresh) if required_project.is_some_and(|project| fresh.project.as_deref() != Some(project)) => {
                        return Err(upstream_error());
                    }
                    Ok(fresh) if fresh.access != account.access => {
                        account = fresh;
                        res = build(&account).send().await.map_err(|_| upstream_error())?;
                    }
                    Err(error) => refresh_error = Some(*error),
                    Ok(fresh) => {
                        account = fresh;
                        let mut error = local_error(
                            AccountErrorKind::AuthExpired,
                            AccountErrorSource::OAuthRefresh,
                            "No renewed credential was available after the upstream returned 401.",
                        );
                        error.http_status = Some(401);
                        refresh_error = Some(error);
                    }
                }
            }
            let first = if res.status().is_success() { res.chunk().await.map_err(|_| upstream_error())? } else { None };
            Ok::<_, AccountError>((res, first, refresh_error))
        };
        let (mut response, first, refresh_error) = match tokio::time::timeout_at(deadline, incoming).await {
            Ok(Ok(result)) => result,
            failure => {
                let timed_out = failure.is_err();
                let classified = failure.ok().and_then(Result::err).unwrap_or_else(|| inference_error("upstream first byte timed out"));
                last_error = if timed_out {
                    StatusCode::GATEWAY_TIMEOUT
                } else if classified.kind == AccountErrorKind::AuthExpired {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::BAD_GATEWAY
                };
                exchange.attempt(
                    &account,
                    previous_session_account,
                    started,
                    began,
                    None,
                    if timed_out { "first_byte_timeout" } else { "upstream_error" },
                );
                exchange.attempt_error(&classified);
                result(&proxy, &account, model, None, false, started, None).await;
                proxy.report_account_error(&account, &classified).await;
                exchange.complete_attempt(Usage::default(), None);
                // Credential preparation failed before any inference request was sent.
                if !policy.switch_on_server_error && !classified.kind.is_blocking() && classified.source != AccountErrorSource::OAuthRefresh
                {
                    exchange.finish(Some(last_error), "upstream_error", Usage::default(), None, None).await;
                    return lane_error(lane, last_error, "upstream request failed");
                }
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
        let mut status = prelude.error.as_ref().map_or_else(|| response.status(), |(error, _)| error.status);
        let received_at = Utc::now();
        let oauth_unavailable = refresh_error.as_ref().is_some_and(|e| e.kind == AccountErrorKind::UpstreamUnavailable);
        let mut pending_quota_body = None;
        let mut quota_reader = None;
        let mut quota_body_verified = None;
        let mut header_reset = None;
        let mut body_reset = None;
        let mut classified = refresh_error;
        if let Some((error, raw)) = prelude.error.as_ref() {
            classified = error.classify(raw, &account, AccountErrorSource::Inference);
            // Auth rejection can be retried before output. Other blocking SSE events
            // terminate the established stream with a sanitized event, not a replay.
            if sse && classified.as_ref().is_some_and(|e| e.kind.is_blocking() && e.kind != AccountErrorKind::AuthExpired) {
                prelude.error = None;
                classified = None;
                status = response.status();
            }
        } else if !status.is_success() {
            let google_quota = lane == Lane::Google && status == StatusCode::TOO_MANY_REQUESTS;
            header_reset = header_cooldown(lane, &headers, received_at);
            if google_quota {
                let unverified_until =
                    header_reset.is_none().then(|| quota_deadline(None, policy.failover_cooldown_secs, received_at).until);
                quota_reader = begin_google_quota_read(&proxy, &account, model, header_reset, unverified_until, received_at).await;
            }
            let diagnostic_deadline =
                if google_quota && header_reset.is_none() { deadline } else { error_prefix_deadline(lane, status, &headers, deadline) };
            let mut prefix = ErrorPrefix::default();
            prefix.read(&mut response, diagnostic_deadline, google_quota).await;
            let diagnostic = prefix.diagnostic();
            if classified.is_none() && (status != StatusCode::FORBIDDEN || prefix.complete || !diagnostic.is_empty()) {
                classified = classify_upstream_http_error(
                    lane,
                    status,
                    &headers,
                    diagnostic,
                    AccountErrorSource::Inference,
                    &account_secrets(&account),
                );
            }
            if google_quota {
                let (reset, verified) = prefix.quota_evidence(Utc::now());
                body_reset = reset;
                quota_body_verified = Some(verified);
                if !prefix.finished() && classified.as_ref().is_some_and(|e| e.kind == AccountErrorKind::QuotaExhausted) {
                    pending_quota_body = header_reset.map(|until| (until, std::mem::take(&mut prefix)));
                }
            }
            prelude.first = prefix.chunks;
            prelude.stream_error = prefix.error;
        }
        let mut limited = classified.as_ref().is_some_and(|e| e.kind == AccountErrorKind::QuotaExhausted);
        let decoded_quota_reset = prelude
            .error
            .as_ref()
            .filter(|_| limited)
            .map(|(error, _)| quota_hint(lane, &headers, error.reset, received_at).map_or(received_at, |hint| hint.until));
        let cooldown = limited.then(|| {
            if let Some((error, _)) = &prelude.error {
                header_reset = header_cooldown(lane, &headers, received_at);
                body_reset = error.reset;
            }
            let hint = merge_quota_hints(lane, &headers, header_reset, body_reset);
            let mut cooldown = quota_deadline(hint, policy.failover_cooldown_secs, Utc::now());
            cooldown.verified_reset &= quota_body_verified.unwrap_or(true);
            cooldown
        });
        let mut quota_until = cooldown.filter(|_| lane == Lane::Google).map(|cooldown| cooldown.until);
        let failover = classified.as_ref().is_some_and(|e| {
            e.kind.is_blocking()
                || limited
                || (e.kind == AccountErrorKind::UpstreamUnavailable && (oauth_unavailable || policy.switch_on_server_error))
        });
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
        if let Some(error) = classified.as_ref() {
            exchange.attempt_error(error);
            let settled = cooldown.filter(|_| pending_quota_body.is_none());
            let body_read = quota_reader.as_ref().zip(quota_body_verified);
            result(&proxy, &account, model, settled, false, started, body_read).await;
            proxy.report_account_error(&account, error).await;
        }
        if pending_quota_body.is_none() {
            if !limited && let Some(reader) = quota_reader.as_ref() {
                discard_google_quota_read(&proxy, &account, reader).await;
            }
            drop(quota_reader.take());
        }
        if limited {
            proxy.schedule_usage_refresh(account.id.clone()).await;
        }
        if failover {
            let has_next = {
                let mut state = proxy.state.write().await;
                let now = Utc::now();
                // Preview on the real scheduler; retries do not increment the round-robin cursor.
                let has_next = pick(&mut state.pool, &exchange, &tried, now).is_some();
                #[cfg(test)]
                if has_next {
                    let _ = AFTER_FAILOVER_PREVIEW.try_with(|hook| hook(&mut state.pool));
                }
                has_next
            };
            if has_next {
                last_decoded_quota_reset = decoded_quota_reset.or(last_decoded_quota_reset);
                let reader = pending_quota_body.map(|(header_until, mut body)| {
                    let proxy = proxy.clone();
                    let model = model.to_owned();
                    let account = account.clone();
                    tokio::spawn(async move {
                        body.read(&mut response, deadline, true).await;
                        body.settle_google_quota(&proxy, &account, &model, &headers, header_until, quota_reader)
                            .await
                            .ok()
                            .map(|cooldown| cooldown.until)
                    })
                });
                if let Some(until) = quota_until {
                    retained_quota.push(RetainedQuota { account, until: Some(until), reader });
                }
                exchange.complete_attempt(prelude.usage, Some(millis(began.elapsed())));
                proxy.log("warn", "upstream request failed over").await;
                continue;
            }
            if let Some((header_until, mut body)) = pending_quota_body {
                body.read(&mut response, deadline, true).await;
                match body.settle_google_quota(&proxy, &account, model, &headers, header_until, quota_reader).await {
                    Ok(cooldown) => quota_until = Some(cooldown.until),
                    Err(error) => {
                        limited = false;
                        quota_until = None;
                        exchange.attempt_error(&error);
                        classified = Some(error);
                    }
                }
                prelude.first = body.chunks;
                prelude.stream_error = body.error;
            }
            if let Some(until) = quota_until {
                retained_quota.push(RetainedQuota { account: account.clone(), until: Some(until), reader: None });
            }
            let (alternate, quota, unavailable) = pick_or_quota(&exchange, &tried, &mut retained_quota).await;
            if alternate.is_some() {
                next_account = alternate;
                last_decoded_quota_reset = decoded_quota_reset.or(last_decoded_quota_reset);
                exchange.complete_attempt(prelude.usage, Some(millis(began.elapsed())));
                proxy.log("warn", "upstream request failed over").await;
                continue;
            }
            if prelude.stream_error.is_none()
                && let Some(reset) = quota
            {
                return exchange.rate_limited(reset, prelude.usage, Some(millis(began.elapsed()))).await;
            }
            if !limited {
                if let Some(response) = unavailable {
                    exchange.complete_attempt(prelude.usage, Some(millis(began.elapsed())));
                    exchange.finish(Some(StatusCode::SERVICE_UNAVAILABLE), "unavailable", Usage::default(), None, None).await;
                    return response;
                }
                if classified.as_ref().is_some_and(|e| e.kind.is_blocking()) {
                    exchange.complete_attempt(prelude.usage, Some(millis(began.elapsed())));
                    exchange.finish(Some(StatusCode::BAD_GATEWAY), "upstream_error", Usage::default(), None, None).await;
                    return lane_error(lane, StatusCode::BAD_GATEWAY, "upstream account rejected this request");
                }
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
            google: (google_stream && !sse && status.is_success()).then(|| GoogleFrames::new(false)),
            google_sse: google_stream && sse && status.is_success(),
            fallback: policy.failover_cooldown_secs,
            usage: Usage::default(),
            usage_buf: Vec::new(),
            sse_frames: SseFrames::default(),
            sse_eof: false,
            sse_terminal: false,
            sse_failed: false,
            request_at: started,
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

fn unavailable_pool(pool: &Pool, lane: Lane, model: &str, required_project: Option<&str>, now: DateTime<Utc>) -> Option<Response> {
    if pool.has_eligible_account(lane, model, required_project, now) {
        return None;
    }
    let kind = pool.uniform_quarantine_kind(lane, model, required_project, now).map_or("accounts_unavailable", |kind| kind.as_str());
    Some((StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({
        "error": {
            "type": kind,
            "message": "All accounts for this provider lane are currently unavailable. Check Fleetix Hub notifications or proxy management for details."
        }
    }))).into_response())
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
    let mut quota_reader = None;
    let mut quota_body_verified = None;
    let fallback = proxy.state.read().await.pool.policy.failover_cooldown_secs;
    let (status, outcome, limited, error) = if let Ok(mut response) = response {
        let status = response.status();
        if !status.is_success() {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
            let headers = response.headers().clone();
            let google_quota = account.lane == Lane::Google && status == StatusCode::TOO_MANY_REQUESTS;
            let now = Utc::now();
            let header_reset = header_cooldown(account.lane, &headers, now);
            if google_quota {
                let unverified_until = header_reset.is_none().then(|| quota_deadline(None, fallback, now).until);
                quota_reader = begin_google_quota_read(&proxy, account, model, None, unverified_until, now).await;
            }
            let wait = if google_quota { deadline } else { error_prefix_deadline(account.lane, status, &headers, deadline) };
            let mut prefix = ErrorPrefix::default();
            prefix.read(&mut response, wait, google_quota).await;
            let body = prefix.diagnostic();
            let error = classify_upstream_http_error(
                account.lane,
                status,
                &headers,
                body,
                AccountErrorSource::Inference,
                &account_secrets(account),
            );
            let limited = error.as_ref().filter(|e| e.kind == AccountErrorKind::QuotaExhausted).map(|_| {
                let (body_reset, verified) = if google_quota { prefix.quota_evidence(Utc::now()) } else { (None, true) };
                quota_body_verified = Some(verified);
                let mut cooldown =
                    quota_deadline(merge_quota_hints(account.lane, &headers, header_reset, body_reset), fallback, Utc::now());
                cooldown.verified_reset &= verified;
                cooldown
            });
            (Some(status), "http_error", limited, error)
        } else {
            let sse = is_sse(response.headers());
            let error = match crate::bounded_body(response, 256 * 1024).await {
                Ok(body) if sse => SseObserver::frame(&body, &mut usage, account),
                Ok(body) => {
                    if let Ok(raw) = serde_json::from_slice(&body) {
                        usage.observe(&raw, account.lane);
                    }
                    None
                }
                Err(message) => Some(local_error(AccountErrorKind::UpstreamUnavailable, AccountErrorSource::Stream, message)),
            };
            (Some(status), if error.is_some() { "stream_error" } else { "success" }, None, error)
        }
    } else {
        (None, "upstream_error", None, Some(inference_error("upstream window start request failed")))
    };
    exchange.attempt(account, None, started, began, status, outcome);
    let success = outcome == "success";
    let limited = limited.or_else(|| {
        error
            .as_ref()
            .filter(|e| e.kind == AccountErrorKind::QuotaExhausted && e.source == AccountErrorSource::Stream)
            .map(|_| quota_deadline(None, fallback, Utc::now()))
    });
    if success || error.is_some() {
        result(&proxy, account, model, limited, success, started, quota_reader.as_ref().zip(quota_body_verified)).await;
    }
    if let Some(error) = error {
        exchange.attempt_error(&error);
        proxy.report_account_error(account, &error).await;
    }
    if limited.is_none()
        && let Some(reader) = quota_reader.as_ref()
    {
        discard_google_quota_read(&proxy, account, reader).await;
    }
    drop(quota_reader);
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

fn inference_error(message: &str) -> AccountError {
    local_error(AccountErrorKind::UpstreamUnavailable, AccountErrorSource::Inference, message)
}

fn error_prefix_deadline(lane: Lane, status: StatusCode, headers: &HeaderMap, deadline: tokio::time::Instant) -> tokio::time::Instant {
    if status == StatusCode::TOO_MANY_REQUESTS && header_cooldown(lane, headers, Utc::now()).is_some() {
        // Give a billing error body a chance to arrive without stalling a quota failover.
        deadline.min(tokio::time::Instant::now() + Duration::from_millis(250))
    } else {
        deadline.min(tokio::time::Instant::now() + Duration::from_secs(3))
    }
}

#[derive(Default)]
struct ErrorPrefix {
    chunks: [Option<Bytes>; 2],
    error: Option<reqwest::Error>,
    complete: bool,
    json_end: Option<usize>,
}

impl ErrorPrefix {
    fn quota_evidence(&self, now: DateTime<Utc>) -> (Option<Cooldown>, bool) {
        let prefix = self.chunks[0].as_deref().unwrap_or_default();
        if let Some(end) = self.json_end {
            let (mut reset, verified) = serde_json::from_slice(&prefix[..end]).map_or((None, false), |raw| google_reset_hint(&raw, now));
            let verified = verified && self.chunks[1].is_none();
            if let Some(hint) = reset.as_mut() {
                hint.verified_reset &= verified;
            }
            (reset, verified)
        } else {
            (None, self.complete && !matches!(prefix.trim_ascii_start().first(), Some(b'{' | b'[')))
        }
    }

    async fn settle_google_quota(
        &self,
        proxy: &Proxy,
        account: &Account,
        model: &str,
        headers: &HeaderMap,
        header: Cooldown,
        reader: Option<QuotaReader>,
    ) -> Result<Cooldown, AccountError> {
        if let Some(error) = classify_upstream_http_error(
            Lane::Google,
            StatusCode::TOO_MANY_REQUESTS,
            headers,
            self.diagnostic(),
            AccountErrorSource::Inference,
            &account_secrets(account),
        )
        .filter(|error| error.kind != AccountErrorKind::QuotaExhausted)
        {
            proxy.report_account_error(account, &error).await;
            if let Some(reader) = reader.as_ref() {
                discard_google_quota_read(proxy, account, reader).await;
            }
            return Err(error);
        }
        let now = Utc::now();
        let (reset, verified) = self.quota_evidence(now);
        let mut merged = merge_quota_hints(Lane::Google, headers, Some(header), reset).unwrap_or(header);
        merged.verified_reset &= verified;
        if let Some(reader) = reader {
            let mut state = proxy.state.write().await;
            if let Some(current) = state.pool.accounts.get_mut(&account.id).filter(|a| a.same_credential(account))
                && current.finish_google_quota_read(&reader, (!verified).then_some(merged.until))
            {
                apply_quota(current, model, merged, now);
            }
        }
        Ok(merged)
    }

    fn finished(&self) -> bool {
        self.complete
            || self.json_end.is_some()
            || self.error.is_some()
            || self.chunks[0].as_ref().is_some_and(|bytes| bytes.len() >= ERROR_BODY_LIMIT)
    }

    fn diagnostic(&self) -> &[u8] {
        // Incomplete free text can end in a partial credential. Forward it, but
        // never use it for operator diagnostics.
        let bytes = self.chunks[0].as_deref().unwrap_or_default();
        if let Some(end) = self.json_end {
            &bytes[..end]
        } else if self.complete || serde_json::from_slice::<Value>(bytes).is_ok() {
            bytes
        } else {
            &[]
        }
    }

    async fn read(&mut self, response: &mut reqwest::Response, deadline: tokio::time::Instant, stop_at_json: bool) {
        if self.finished() {
            return;
        }
        let mut prefix = self.chunks[0].take().map_or_else(Vec::new, |bytes| bytes.to_vec());
        let mut boundary = JsonBoundary::default();
        let mut scanned = 0;
        let mut object = None;
        loop {
            if stop_at_json {
                if object.is_none() {
                    let trimmed = prefix[scanned..].trim_ascii_start();
                    scanned = prefix.len() - trimmed.len();
                    object = trimmed.first().map(|b| matches!(b, b'{' | b'['));
                }
                if object == Some(true) {
                    if let Some(end) = boundary.scan(&prefix[scanned..]) {
                        self.json_end = Some(scanned + end);
                        break;
                    }
                    scanned = prefix.len();
                }
            }
            if prefix.len() >= ERROR_BODY_LIMIT {
                break;
            }
            let chunk = match tokio::time::timeout_at(deadline, response.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Err(err)) => {
                    self.error = Some(err);
                    break;
                }
                Ok(Ok(None)) => {
                    self.complete = true;
                    break;
                }
                Err(_) => break,
            };
            let take = chunk.len().min(ERROR_BODY_LIMIT - prefix.len());
            prefix.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                self.chunks[1] = Some(chunk.slice(take..));
            }
        }
        self.chunks[0] = (!prefix.is_empty()).then(|| prefix.into());
    }
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

fn is_rate_limit_reset(name: &str) -> bool {
    (name.starts_with("anthropic-ratelimit-") || name.starts_with("x-ratelimit-")) && (name.ends_with("-reset") || name.contains("-reset-"))
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
        let strict_source = (lane == Lane::Google || fallback_can_verify) && single(name);
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
    let retry_after = get("retry-after")
        .and_then(|raw| {
            raw.parse::<i64>().ok().filter(|n| *n >= 0).map(|n| verify(upstream_delay(now, n as f64), digits(raw))).or_else(|| {
                let at = DateTime::parse_from_rfc2822(raw).ok()?.with_timezone(&Utc);
                let strict = raw == at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
                Some(verify(clamp(at), strict))
            })
        })
        .map(|at| verify(at, single("retry-after")));
    let resets = headers
        .iter()
        .filter(|(name, _)| is_rate_limit_reset(name.as_str()))
        .map(|(name, value)| value.to_str().ok().and_then(|raw| parse_reset(name.as_str(), raw)));
    if lane == Lane::Google {
        // A bad reset header invalidates proof even if another header wins for scheduling.
        let mut verified = fallback_can_verify || retry_after.is_some_and(|hint| hint.verified_reset);
        let reset = resets.inspect(|hint| verified &= hint.is_some_and(|hint| hint.verified_reset)).flatten().reduce(Cooldown::merge);
        return retry_after.or(reset).map(|at| verify(at, verified));
    }
    if retry_after.is_some() {
        return retry_after;
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
    resets.flatten().reduce(Cooldown::merge)
}

#[cfg(test)]
fn body_reset_hint(body: &[u8], now: DateTime<Utc>) -> Option<Cooldown> {
    let raw: Value = serde_json::from_slice(body).ok()?;
    google_reset_hint(&raw, now).0
}

fn google_detail_kind(detail: &Value) -> Option<&str> {
    detail["@type"].as_str()?.rsplit('/').next()
}

fn google_reset_delay(value: &Value, now: DateTime<Utc>) -> Option<Cooldown> {
    let (secs, strict) = duration_seconds(value.as_str()?)?;
    let mut hint = upstream_delay(now, secs);
    hint.verified_reset &= strict;
    Some(hint)
}

fn google_reset_timestamp(value: &Value, now: DateTime<Utc>) -> Option<Cooldown> {
    value.as_str()?.parse::<DateTime<Utc>>().ok().map(|at| clamp_cooldown(now, at))
}

fn google_reset_hint(raw: &Value, now: DateTime<Utc>) -> (Option<Cooldown>, bool) {
    fn collect(raw: &Value, now: DateTime<Utc>, verified: &mut bool) -> Option<Cooldown> {
        if let Value::Array(items) = raw {
            return items.iter().filter_map(|item| collect(item, now, verified)).reduce(Cooldown::merge);
        }
        let error = raw.get("error")?;
        // An item may omit its status in a 429 body, but an explicit non-quota error cannot attest a quota reset.
        if !error.get("code").is_none_or(|code| matches!(code.as_u64(), Some(8 | 429)))
            || !error.get("status").is_none_or(|status| matches!(status.as_str(), Some("RESOURCE_EXHAUSTED" | "QUOTA_EXHAUSTED")))
        {
            return None;
        }
        let mut parse = |value: Option<&Value>, parse: fn(&Value, DateTime<Utc>) -> Option<Cooldown>| {
            let hint = parse(value?, now);
            *verified &= hint.is_some_and(|hint| hint.verified_reset);
            hint
        };
        error
            .get("details")?
            .as_array()?
            .iter()
            .flat_map(|detail| match google_detail_kind(detail) {
                Some("google.rpc.RetryInfo" | "RetryInfo") => [parse(detail.get("retryDelay"), google_reset_delay), None],
                Some("google.rpc.ErrorInfo" | "ErrorInfo") => [
                    parse(detail.pointer("/metadata/quotaResetDelay"), google_reset_delay),
                    parse(detail.pointer("/metadata/quotaResetTimeStamp"), google_reset_timestamp),
                ],
                _ => [None, None],
            })
            .flatten()
            .reduce(Cooldown::merge)
    }
    let mut verified = true;
    let mut reset = collect(raw, now, &mut verified);
    if let Some(hint) = reset.as_mut() {
        hint.verified_reset &= verified;
    }
    (reset, verified)
}

fn merge_quota_hints(lane: Lane, headers: &HeaderMap, header: Option<Cooldown>, body: Option<Cooldown>) -> Option<Cooldown> {
    let mut cooldown = header.into_iter().chain(body).reduce(Cooldown::merge)?;
    // A later body hint can extend the cooldown, but cannot rehabilitate an unusable header.
    let reset_header = headers.contains_key(header::RETRY_AFTER)
        || (lane == Lane::Google && headers.keys().any(|name| is_rate_limit_reset(name.as_str())));
    cooldown.verified_reset &= !reset_header || header.is_some_and(|hint| hint.verified_reset);
    Some(cooldown)
}

fn quota_hint(lane: Lane, headers: &HeaderMap, body: Option<Cooldown>, now: DateTime<Utc>) -> Option<Cooldown> {
    merge_quota_hints(lane, headers, header_cooldown(lane, headers, now), body)
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

async fn result(
    proxy: &Proxy,
    account: &Account,
    model: &str,
    cooldown: Option<Cooldown>,
    success: bool,
    request_at: DateTime<Utc>,
    body_read: Option<(&QuotaReader, bool)>,
) {
    let mut state = proxy.state.write().await;
    let Some(current) = state.pool.accounts.get_mut(&account.id) else { return };
    let now = Utc::now();
    let same_secret = current.same_secret(account);
    if same_secret {
        current.last_used = Some(now);
        if success {
            current.success = current.success.saturating_add(1);
            if current.last_error.as_ref().is_none_or(|e| e.occurred_at <= request_at) && current.clear_account_error() {
                current.probe_failed = false;
                proxy.notify_hub_sync();
            }
        } else {
            current.failure = current.failure.saturating_add(1);
        }
    }
    if let Some(until) = cooldown
        && (same_secret || (account.lane == Lane::Google && current.same_credential(account)))
        && body_read.is_none_or(|(reader, verified)| current.finish_google_quota_read(reader, (!verified).then_some(until.until)))
    {
        apply_quota(current, model, until, now);
    }
}

async fn begin_google_quota_read(
    proxy: &Proxy,
    account: &Account,
    model: &str,
    provisional: Option<Cooldown>,
    unverified_until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<QuotaReader> {
    proxy
        .state
        .write()
        .await
        .pool
        .accounts
        .get_mut(&account.id)
        .filter(|a| a.same_credential(account))
        .map(|current| current.begin_google_quota_read(model, provisional, unverified_until, now))
}

async fn extend_quota(proxy: &Proxy, account: &Account, model: &str, until: Cooldown) {
    let mut state = proxy.state.write().await;
    if let Some(current) = state.pool.accounts.get_mut(&account.id).filter(|a| a.same_credential(account)) {
        apply_quota(current, model, until, Utc::now());
    }
}

async fn discard_google_quota_read(proxy: &Proxy, account: &Account, reader: &QuotaReader) {
    let mut state = proxy.state.write().await;
    if let Some(current) = state.pool.accounts.get_mut(&account.id).filter(|a| a.same_credential(account)) {
        current.finish_google_quota_read(reader, None);
    }
}

fn apply_quota(account: &mut Account, model: &str, until: Cooldown, now: DateTime<Utc>) {
    if let Some(scope) = model_cooldown_scope(account.lane, model) {
        account.scoped_cooldowns.retain(|_, c| c.until > now);
        account.scoped_cooldowns.entry(scope).and_modify(|old| *old = old.merge(until)).or_insert(until);
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
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
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
        Some((Self { status, reset: google_reset_hint(raw, Utc::now()).0 }, raw))
    }

    fn classify(self, raw: &Value, account: &Account, source: AccountErrorSource) -> Option<AccountError> {
        let mut classified = classify_upstream_http_error(
            Lane::Google,
            self.status,
            &HeaderMap::new(),
            raw.to_string().as_bytes(),
            source,
            &account_secrets(account),
        )?;
        // A generic in-band 403 can be specific to the requested resource.
        if classified.kind == AccountErrorKind::PermissionDenied
            && !raw.pointer("/error/details").is_some_and(Value::is_array)
            && source == AccountErrorSource::Inference
        {
            return None;
        }
        classified.source = source;
        Some(classified)
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
    google_sse: bool,
    fallback: u64,
    usage: Usage,
    usage_buf: Vec<u8>,
    sse_frames: SseFrames,
    sse_eof: bool,
    sse_terminal: bool,
    sse_failed: bool,
    request_at: DateTime<Utc>,
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
            if let Some((error, detail)) = GoogleError::from_frame(&raw)
                && let Some(exchange) = &mut self.guard.0
            {
                let classified = error.classify(detail, &self.account, AccountErrorSource::Stream);
                let now = Utc::now();
                let cooldown = classified
                    .as_ref()
                    .filter(|e| e.kind == AccountErrorKind::QuotaExhausted)
                    .map(|_| quota_deadline(quota_hint(Lane::Google, self.upstream.headers(), error.reset, now), self.fallback, now));
                let first_error = self.guard.1.is_none();
                if first_error {
                    result(&self.proxy, &self.account, &exchange.model, cooldown, false, self.request_at, None).await;
                    if let Some(classified) = &classified {
                        exchange.attempt_error(classified);
                        self.proxy.report_account_error(&self.account, classified).await;
                    }
                    if let Some(last) = exchange.attempts.last_mut() {
                        last.status_code = Some(i32::from(error.status.as_u16()));
                        last.outcome = error.outcome().into();
                    }
                    self.guard.1 = Some((error, self.usage, millis(self.started.elapsed())));
                } else if let Some(until) = cooldown {
                    extend_quota(&self.proxy, &self.account, &exchange.model, until).await;
                }
                if cooldown.is_some() {
                    if first_error {
                        self.proxy.schedule_usage_refresh(self.account.id.clone()).await;
                    }
                    normalize_google_quota(&mut raw, &exchange.model, now);
                } else if let Some(error) = classified.filter(|e| e.kind.is_blocking()) {
                    raw = stream_error_body(error.kind);
                } else {
                    return frame.bytes;
                }
                let mut bytes = frame.bytes.to_vec();
                bytes.splice(span, raw.to_string().bytes());
                return bytes.into();
            }
        }
        frame.bytes
    }

    async fn record_stream_error(&mut self, error: AccountError) {
        self.sse_failed = true;
        let limited = (error.kind == AccountErrorKind::QuotaExhausted).then(|| quota_deadline(None, self.fallback, Utc::now()));
        if let Some(exchange) = &mut self.guard.0 {
            exchange.attempt_error(&error);
            if let Some(last) = exchange.attempts.last_mut() {
                last.outcome = "stream_error".into();
            }
        }
        let model = self.guard.0.as_ref().map_or("", |exchange| exchange.model.as_str());
        result(&self.proxy, &self.account, model, limited, false, self.request_at, None).await;
        self.proxy.report_account_error(&self.account, &error).await;
        if limited.is_some() {
            self.proxy.schedule_usage_refresh(self.account.id.clone()).await;
        }
    }

    async fn upstream_chunk(&mut self) -> Result<Option<Bytes>, reqwest::Error> {
        if let Some(chunk) = self.first[0].take().or_else(|| self.first[1].take()) {
            Ok(Some(chunk))
        } else if let Some(err) = self.stream_error.take() {
            // Let the downstream dispatcher flush the buffered prefix before it aborts the body.
            tokio::task::yield_now().await;
            Err(err)
        } else {
            self.upstream.chunk().await
        }
    }

    async fn next_sse_chunk(&mut self) -> Result<Option<Bytes>, reqwest::Error> {
        if self.sse_terminal {
            return Ok(None);
        }
        loop {
            let (frame, error) = match self.sse_frames.take(self.sse_eof) {
                Some((frame, true)) => {
                    let error = SseObserver::frame(&frame, &mut self.usage, &self.account);
                    if self.google_sse && !error.as_ref().is_some_and(|e| e.kind.is_blocking()) {
                        let mut lines = GoogleFrames::new(true);
                        lines.push(frame.clone());
                        let mut rewritten = Vec::new();
                        while let Some(line) = lines.next(true) {
                            rewritten.extend_from_slice(&self.google_frame(line).await);
                        }
                        (rewritten.into(), error.filter(|_| self.guard.1.is_none()))
                    } else {
                        (frame, error)
                    }
                }
                Some((bytes, false)) => return Ok(Some(bytes)),
                None if self.sse_eof => return Ok(None),
                None => {
                    if let Some(chunk) = self.upstream_chunk().await? {
                        self.sse_frames.push(chunk);
                    } else {
                        self.sse_eof = true;
                    }
                    continue;
                }
            };
            if let Some(error) = error {
                let kind = error.kind;
                let terminal = kind.is_blocking();
                if self.status.is_success() && (!self.sse_failed || terminal) {
                    self.record_stream_error(error).await;
                }
                if terminal {
                    self.sse_terminal = true;
                    let body = stream_error_body(kind);
                    return Ok(Some(Bytes::from(format!("event: error\ndata: {body}\n\n"))));
                }
            }
            return Ok(Some(frame));
        }
    }

    async fn next(mut self) -> Result<Option<(Bytes, Self)>, reqwest::Error> {
        let chunk = if self.sse {
            self.next_sse_chunk().await
        } else {
            match self.next_frame().await {
                Ok(Some(frame)) => Ok(Some(self.google_frame(frame).await)),
                Ok(None) => Ok(None),
                Err(err) => Err(err),
            }
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
            if !self.sse && self.google.is_none() {
                if self.response_size <= 1024 * 1024 {
                    self.usage_buf.extend_from_slice(data);
                } else {
                    self.usage_buf.clear();
                }
            }
        }
        match chunk {
            Ok(Some(data)) => Ok(Some((data, self))),
            done => {
                if !self.sse
                    && let Ok(raw) = serde_json::from_slice(&self.usage_buf)
                {
                    self.usage.observe(&raw, self.account.lane);
                }
                if self.status.is_success() && self.guard.1.is_none() {
                    if done.is_err() && !self.sse_failed {
                        self.record_stream_error(local_error(
                            AccountErrorKind::UpstreamUnavailable,
                            AccountErrorSource::Stream,
                            "upstream response stream broke",
                        ))
                        .await;
                    }
                    if !self.sse_failed {
                        result(&self.proxy, &self.account, "", None, true, self.request_at, None).await;
                    }
                }
                if let Some(mut exchange) = self.guard.0.take() {
                    let outcome = if let Some((error, _, _)) = self.guard.1 {
                        error.outcome()
                    } else if done.is_err() || self.sse_failed {
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

fn stream_error_body(kind: AccountErrorKind) -> Value {
    json!({"error": {
        "type": kind.as_str(),
        "message": "The provider stopped this stream. Check Fleetix Hub notifications for details."
    }})
}

// Hold whole events, not HTTP chunks: an upstream error can start in one
// chunk and become classifiable only in a later chunk.
#[derive(Default)]
struct SseFrames {
    pending: Bytes,
    frame: Vec<u8>,
    line_has_data: bool,
    passthrough: bool,
    after_cr: bool,
}

fn sse_line_break(bytes: &[u8]) -> Option<(usize, usize)> {
    let at = bytes.iter().position(|b| matches!(b, b'\r' | b'\n'))?;
    let crlf = bytes[at] == b'\r' && bytes.get(at + 1) == Some(&b'\n');
    Some((at, at + 1 + usize::from(crlf)))
}

impl SseFrames {
    fn push(&mut self, chunk: Bytes) {
        debug_assert!(self.pending.is_empty() || self.pending.as_ref() == b"\r");
        self.pending = if self.pending.is_empty() {
            chunk
        } else {
            // An isolated trailing CR might be the first byte of CRLF.
            let mut joined = Vec::with_capacity(chunk.len() + 1);
            joined.push(b'\r');
            joined.extend_from_slice(&chunk);
            joined.into()
        };
    }

    // The bool indicates a complete frame eligible for inspection, not raw passthrough bytes.
    fn take(&mut self, eof: bool) -> Option<(Bytes, bool)> {
        const MAX_EVENT_BYTES: usize = 1024 * 1024;
        if !self.pending.is_empty() && std::mem::take(&mut self.after_cr) && self.pending[0] == b'\n' {
            return Some((self.pending.split_to(1), false));
        }
        let mut forwarded = 0;
        while forwarded < self.pending.len() {
            let rest = &self.pending[forwarded..];
            let (len, end) = sse_line_break(rest).unwrap_or((rest.len(), rest.len()));
            let boundary = len == 0 && !self.line_has_data;
            let trailing_cr = !eof && len + 1 == rest.len() && rest[len] == b'\r';
            // A blank line ending in a CR already completes the event. If its
            // optional LF arrives later, forward it without treating it as another line.
            let wait_for_lf = trailing_cr && !boundary && !self.passthrough;
            if !self.passthrough && self.frame.len() + end > MAX_EVENT_BYTES {
                self.passthrough = true;
                if !self.frame.is_empty() {
                    return Some((std::mem::take(&mut self.frame).into(), false));
                }
                continue;
            }
            let end = if wait_for_lf { len } else { end };
            if end == 0 {
                break;
            }
            let line_ended = end > len;
            self.line_has_data = !line_ended && (self.line_has_data || len > 0);
            self.after_cr = trailing_cr && line_ended;
            if self.passthrough {
                forwarded += end;
                if boundary {
                    self.passthrough = false;
                    break;
                }
            } else {
                self.frame.extend_from_slice(&self.pending.split_to(end));
                if boundary {
                    return Some((std::mem::take(&mut self.frame).into(), true));
                }
            }
        }
        if forwarded > 0 {
            Some((self.pending.split_to(forwarded), false))
        } else {
            (eof && !self.frame.is_empty()).then(|| (std::mem::take(&mut self.frame).into(), true))
        }
    }
}

#[derive(Default)]
struct SseObserver {
    event: String,
    data: Vec<u8>,
}

impl SseObserver {
    fn frame(frame: &[u8], usage: &mut Usage, account: &Account) -> Option<AccountError> {
        let mut observer = Self::default();
        let mut rest = frame.strip_prefix(b"\xef\xbb\xbf").unwrap_or(frame);
        let mut first_error = None;
        while !rest.is_empty() {
            let (len, end) = sse_line_break(rest).unwrap_or((rest.len(), rest.len()));
            if let Some(error) = observer.observe(&rest[..len], usage, account) {
                if error.kind.is_blocking() {
                    return Some(error);
                }
                first_error.get_or_insert(error);
            }
            rest = &rest[end..];
        }
        observer.flush(usage, account).or(first_error)
    }

    fn observe(&mut self, line: &[u8], usage: &mut Usage, account: &Account) -> Option<AccountError> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return self.flush(usage, account);
        }
        if let Some(event) = line.strip_prefix(b"event:") {
            self.event = match event.trim_ascii() {
                b"error" => "error".into(),
                b"response.failed" => "response.failed".into(),
                _ => String::new(),
            };
        } else if let Some(data) = line.strip_prefix(b"data:") {
            self.data.extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
            self.data.push(b'\n');
            if let Ok(raw) = serde_json::from_slice::<Value>(data) {
                usage.observe(&raw, account.lane);
                if let Some(error) = classify_sse_error(account.lane, &self.event, data, &account_secrets(account)) {
                    return Some(error);
                }
            }
        }
        None
    }

    fn flush(&mut self, usage: &mut Usage, account: &Account) -> Option<AccountError> {
        if let Ok(raw) = serde_json::from_slice::<Value>(&self.data) {
            usage.observe(&raw, account.lane);
        }
        let error = classify_sse_error(account.lane, &self.event, &self.data, &account_secrets(account));
        self.event.clear();
        self.data.clear();
        error
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
                let mut status = None;
                let mut outcome = "disconnected";
                if let Some(last) = exchange.attempts.last_mut() {
                    if last.outcome == "stream_error" {
                        outcome = "stream_error";
                        status = last.status_code.and_then(|n| StatusCode::from_u16(n as u16).ok());
                    } else {
                        last.outcome = "disconnected".into();
                    }
                }
                exchange.finish(status, outcome, Usage::default(), None, None).await;
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

    fn attempt_error(&mut self, error: &AccountError) {
        if let Some(last) = self.attempts.last_mut() {
            last.error_message = Some(error.summary());
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
            error_message: last.and_then(|a| a.error_message.clone()),
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
