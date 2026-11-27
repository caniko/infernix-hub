use std::{
    collections::BTreeSet,
    fs,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use contracts::providers::{ApiKeyOrder, QuotaWindowKind, RouteStrategy, RoutingPolicy};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    Proxy, bounded_body,
    clients::Client,
    config::{matches_secret, read_text, write_private},
    openai_account,
    pool::{Account, AccountOverride, Lane, Pool, enforces_quota, filename, parse_credentials, text},
    provider_json,
    router::bearer,
};

type Reply = Result<Json<Value>, Failure>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Failure(pub StatusCode, pub &'static str);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn bad(message: &'static str) -> Failure {
    Failure(StatusCode::BAD_REQUEST, message)
}
fn missing() -> Failure {
    Failure(StatusCode::NOT_FOUND, "account not found")
}
fn upstream() -> Failure {
    Failure(StatusCode::BAD_GATEWAY, "provider request failed")
}
fn disk() -> Failure {
    Failure(StatusCode::INTERNAL_SERVER_ERROR, "could not update local configuration")
}
fn ok(value: Value) -> Reply {
    Ok(Json(value))
}

pub(crate) fn routes(proxy: Arc<Proxy>, v8: bool) -> Router<Arc<Proxy>> {
    let routes = if v8 {
        Router::new()
            .route("/config", get(config))
            .route("/config/routing", axum::routing::patch(patch_config))
            .route("/config/routing/strategy", get(strategy).put(set_strategy))
            .route("/usage/windows", get(windows))
            .route("/usage/refresh", post(refresh))
            .route("/usage/reset-credit", post(reset_credit))
            .route("/usage/start-window", post(start_window))
            .route("/credentials", get(inventory).post(upload).delete(delete_credential))
            .route("/credentials/status", axum::routing::patch(edit_credential))
            .route("/credentials/fields", axum::routing::patch(edit_credential))
            .route("/credentials/refresh", post(renew_credentials))
            .route("/credentials/models", get(models))
            .route("/models", get(models))
            .route("/routing/policy", get(policy).put(set_policy))
            .route("/routing/override", post(set_override))
            .route("/routing/preferred-auth", get(get_override).put(set_override))
            .route("/routing/move", post(move_session))
            .route("/routing/move-session", post(move_session))
            .route("/observability/captures/lease", get(capture_lease).put(set_capture_lease))
            .route("/observability/captures", get(captures).delete(clear_captures))
            .route("/observability/captures/ack", post(ack_captures))
            .route("/clients", get(clients))
            .route("/clients/{client_id}/apply", post(apply_client))
            .route("/clients/{client_id}/undo", post(undo_client))
            .route("/clients/{client_id}/reapply", post(apply_client))
            .route("/logs", get(logs))
            .route("/observability/logs", get(logs))
            .route("/server/shutdown", post(shutdown))
    } else {
        Router::new()
            .route("/auth-files", get(auth_files).delete(delete_credential))
            .route("/auth-files/status", axum::routing::patch(edit_credential))
            .route("/auth-files/fields", axum::routing::patch(edit_credential))
            .route("/auth-files/models", get(models))
            .route("/api-call", post(api_call))
            .route("/reset-quota", post(reset_quota))
            .route("/routing/strategy", get(strategy).put(set_strategy))
            .route("/logs", get(logs))
            .route("/debug", get(config))
    };
    routes
        .route("/model-eligibility", get(model_eligibility))
        .layer(middleware::from_fn_with_state(proxy.clone(), move |state, req, next| authenticate(state, req, next, v8)))
        .fallback(move |req: Request| async move {
            if bearer(req.headers()).is_some_and(|key| matches_secret(key, &proxy.config.usage_key)) {
                if v8 { invalid_credentials() } else { usage_read_only() }.into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagementScope {
    Full,
    UsageReadOnly,
}

fn invalid_credentials() -> Failure {
    Failure(StatusCode::UNAUTHORIZED, "invalid management credentials")
}

fn usage_read_only() -> Failure {
    Failure(StatusCode::FORBIDDEN, "usage key is read-only")
}

async fn authenticate(State(proxy): State<Arc<Proxy>>, mut req: Request, next: Next, v8: bool) -> Response {
    let Some(key) = bearer(req.headers()) else { return invalid_credentials().into_response() };
    let target = (req.method().as_str(), req.uri().path());
    let eligibility = target == ("GET", "/model-eligibility");
    let scope = if matches_secret(key, &proxy.config.management_key) {
        ManagementScope::Full
    } else if (!v8 || eligibility) && matches_secret(key, &proxy.config.usage_key) {
        if !eligibility && !matches!(target, ("GET", "/auth-files" | "/auth-files/models") | ("POST", "/api-call")) {
            return usage_read_only().into_response();
        }
        ManagementScope::UsageReadOnly
    } else {
        return invalid_credentials().into_response();
    };
    req.extensions_mut().insert(scope);
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    for (key, value) in [
        ("x-cpa-version", env!("CARGO_PKG_VERSION")),
        ("x-cpa-commit", option_env!("FLEETIX_PROXY_BUILD_COMMIT").unwrap_or("unknown")),
        ("x-cpa-build-date", option_env!("FLEETIX_PROXY_BUILD_DATE").unwrap_or("unknown")),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
    ] {
        if let Ok(value) = header::HeaderValue::from_str(value) {
            headers.insert(key, value);
        }
    }
    response
}

fn strategy_name(strategy: RouteStrategy) -> &'static str {
    match strategy {
        RouteStrategy::SoonestReset => "soonest-reset",
        RouteStrategy::FillFirst => "fill-first",
        RouteStrategy::RoundRobin => "round-robin",
        RouteStrategy::WeightedRoundRobin => "weighted-round-robin",
        RouteStrategy::CapacityBalanced => "capacity-balanced",
    }
}

fn parse_strategy(raw: &Value) -> Result<RouteStrategy, Failure> {
    let raw = raw.as_str().or_else(|| text(raw, &["strategy", "value"])).ok_or_else(|| bad("strategy is required"))?;
    match raw.replace('_', "-").as_str() {
        "soonest-reset" => Ok(RouteStrategy::SoonestReset),
        "fill-first" => Ok(RouteStrategy::FillFirst),
        "round-robin" => Ok(RouteStrategy::RoundRobin),
        "weighted-round-robin" => Ok(RouteStrategy::WeightedRoundRobin),
        "capacity-balanced" => Ok(RouteStrategy::CapacityBalanced),
        _ => Err(bad("unknown routing strategy")),
    }
}

async fn strategy(State(proxy): State<Arc<Proxy>>) -> Reply {
    ok(json!(strategy_name(proxy.state.read().await.pool.policy.strategy)))
}

async fn set_strategy(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Value>) -> Reply {
    let strategy = parse_strategy(&raw)?;
    let mut state = proxy.state.write().await;
    let mut policy = state.pool.policy.clone();
    policy.strategy = strategy;
    update_policy(&mut state.pool, policy)?;
    ok(json!({"status":"ok", "strategy":strategy_name(strategy)}))
}

fn config_view(pool: &Pool) -> Value {
    json!({"routing": {
        "strategy": strategy_name(pool.policy.strategy), "session-affinity": pool.policy.session_affinity_secs > 0,
        "prefer-auth-kind": if pool.policy.api_key_order == ApiKeyOrder::KeysFirst { "api-key" } else { "oauth" },
    }, "debug": false})
}

async fn config(State(proxy): State<Arc<Proxy>>) -> Response {
    let state = proxy.state.read().await;
    let tag = format!("\"{}\"", state.pool.revision);
    ([(header::ETAG, tag)], Json(config_view(&state.pool))).into_response()
}

async fn patch_config(State(proxy): State<Arc<Proxy>>, headers: HeaderMap, Json(patch): Json<Map<String, Value>>) -> Reply {
    let mut state = proxy.state.write().await;
    if headers.get(header::IF_MATCH).and_then(|tag| tag.to_str().ok()) != Some(format!("\"{}\"", state.pool.revision).as_str()) {
        return Err(Failure(StatusCode::PRECONDITION_FAILED, "routing revision changed"));
    }
    let mut policy = state.pool.policy.clone();
    for (key, value) in patch {
        match key.as_str() {
            "strategy" => policy.strategy = parse_strategy(&value)?,
            "session-affinity" => {
                policy.session_affinity_secs = if value.as_bool().ok_or_else(|| bad("expected affinity boolean"))? {
                    RoutingPolicy::default().session_affinity_secs
                } else {
                    0
                }
            }
            "prefer-auth-kind" => {
                policy.api_key_order = match value.as_str() {
                    Some("api-key") => ApiKeyOrder::KeysFirst,
                    Some("oauth") => ApiKeyOrder::KeysLast,
                    _ => return Err(bad("unknown preferred credential type")),
                }
            }
            _ => return Err(bad("unknown configuration field")),
        }
    }
    update_policy(&mut state.pool, policy)?;
    ok(json!({"status":"ok"}))
}

pub(crate) fn update_policy(pool: &mut Pool, policy: RoutingPolicy) -> Result<(), Failure> {
    if let Some(id) = policy.preferred_account_id.as_deref() {
        pool.find(id).filter(|a| !a.disabled).ok_or_else(missing)?;
    }
    if policy.first_byte_timeout_secs == 0
        || policy.first_byte_timeout_secs > 600
        || policy.session_affinity_secs > 604800
        || policy.failover_cooldown_secs > 604800
    {
        return Err(bad("routing timeout outside supported range"));
    }
    if pool.policy != policy {
        if pool.policy.strategy != policy.strategy {
            pool.cursors.clear();
        }
        pool.policy = policy;
        pool.revision += 1;
    }
    Ok(())
}

async fn policy(State(proxy): State<Arc<Proxy>>) -> Reply {
    ok(json!(proxy.state.read().await.pool.policy))
}

async fn set_policy(State(proxy): State<Arc<Proxy>>, Json(mut raw): Json<Value>) -> Reply {
    if let Some(value) = raw.get_mut("strategy") {
        *value = json!(parse_strategy(value)?);
    }
    let value = serde_json::from_value(raw).map_err(|_| bad("invalid routing policy"))?;
    let mut state = proxy.state.write().await;
    update_policy(&mut state.pool, value)?;
    ok(json!(state.pool.policy))
}

pub(crate) fn override_account(pool: &mut Pool, lane: Option<Lane>, id: Option<&str>) -> Result<(), Failure> {
    let account = id
        .filter(|s| !s.is_empty())
        .map(|id| {
            pool.find(id)
                .filter(|a| !a.disabled && lane.is_none_or(|lane| a.lane == lane))
                .map(|a| (a.lane, a.id.clone()))
                .ok_or_else(missing)
        })
        .transpose()?;
    let changed = if let Some((lane, id)) = account {
        pool.overrides.insert(lane, id.clone()).as_deref() != Some(id.as_str())
    } else if let Some(lane) = lane {
        let had_override = pool.overrides.remove(&lane).is_some();
        if pool.policy.preferred_account_id.as_deref().and_then(|id| pool.find(id)).is_some_and(|a| a.lane == lane) {
            pool.policy.preferred_account_id = None;
            true
        } else {
            had_override
        }
    } else {
        let changed = !pool.overrides.is_empty() || pool.policy.preferred_account_id.is_some();
        pool.overrides.clear();
        pool.policy.preferred_account_id = None;
        changed
    };
    if changed {
        pool.revision += 1;
    }
    Ok(())
}

async fn set_override(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Map<String, Value>>) -> Reply {
    let raw = Value::Object(raw);
    let lane = text(&raw, &["provider"]).map(|kind| Lane::parse(kind).ok_or_else(|| bad("unknown provider"))).transpose()?;
    if raw.get("provider").is_none()
        && raw.get("account_id").is_none()
        && raw.get("auth_index").is_none()
        && raw.get("preferred_account_id").is_none()
    {
        return Err(bad("account selection is required"));
    }
    let id = text(&raw, &["auth_index", "account_id", "preferred_account_id"]);
    let mut state = proxy.state.write().await;
    override_account(&mut state.pool, lane, id)?;
    ok(preferred(&state.pool))
}

async fn get_override(State(proxy): State<Arc<Proxy>>) -> Reply {
    ok(preferred(&proxy.state.read().await.pool))
}

fn preferred(pool: &Pool) -> Value {
    let preferred = [Lane::Claude, Lane::Codex, Lane::Google, Lane::Go]
        .into_iter()
        .filter_map(|lane| {
            let index = &pool.find(pool.preferred(lane)?)?.auth_index;
            Some((lane.name().into(), json!(index)))
        })
        .collect::<Map<String, Value>>();
    json!({"preferred":preferred})
}

async fn move_session(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Value>) -> Reply {
    let session = text(&raw, &["session_id"]).ok_or_else(|| bad("session_id is required"))?;
    let target = text(&raw, &["auth_index", "target_account_id", "account_id"]).ok_or_else(|| bad("target account is required"))?;
    let moved = proxy.state.write().await.pool.move_session(session, target, raw["include_subagents"].as_bool().unwrap_or(true)).map_err(
        |message| Failure(if message == "session affinity is disabled" { StatusCode::CONFLICT } else { StatusCode::NOT_FOUND }, message),
    )?;
    ok(json!({"status":"ok","bound":moved}))
}

async fn inventory(State(proxy): State<Arc<Proxy>>) -> Reply {
    let state = proxy.state.read().await;
    ok(json!({"files":state.pool.accounts.values().map(|a| a.inventory(&state.pool)).collect::<Vec<_>>()}))
}

async fn auth_files(State(proxy): State<Arc<Proxy>>) -> Reply {
    let state = proxy.state.read().await;
    ok(json!({"files":state.pool.accounts.values().filter(|a| !a.api_key).map(|a| a.inventory(&state.pool)).collect::<Vec<_>>()}))
}

fn usage(pool: &Pool) -> Value {
    let picks = [Lane::Claude, Lane::Codex, Lane::Google, Lane::Go].map(|lane| (lane, pool.preview(lane)));
    let credentials: Vec<_> = pool
        .accounts
        .values()
        .map(|a| {
            let mut row = a.quota(pool);
            row["next_pick"] = json!(picks.iter().any(|(lane, id)| *lane == a.lane && id.as_deref() == Some(&a.id)));
            row
        })
        .collect();
    json!({"strategy":strategy_name(pool.policy.strategy), "credentials":credentials})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsQuery {
    lane: Option<String>,
    model: Option<String>,
}

async fn model_eligibility(State(proxy): State<Arc<Proxy>>, query: Result<Query<WindowsQuery>, QueryRejection>) -> Reply {
    let Query(query) = query.map_err(|_| bad("invalid model eligibility query"))?;
    let lane = match query.lane.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("claudeagent") => Some(Lane::Claude),
        Some("go") => Some(Lane::Go),
        Some(alias) => Lane::parse(alias),
        None => None,
    }
    .ok_or_else(|| bad("missing or unknown lane"))?;
    let model = query.model.as_deref();
    if model.is_some_and(|model| !crate::pool::bounded_identifier(model)) {
        return Err(bad("invalid wire model"));
    }
    ok(proxy.state.read().await.pool.model_eligibility_summary(lane, model, Utc::now()))
}

async fn windows(State(proxy): State<Arc<Proxy>>, query: Result<Query<WindowsQuery>, QueryRejection>) -> Reply {
    let Query(query) = query.map_err(|_| bad("invalid usage windows query"))?;
    match (query.lane.as_deref(), query.model.as_deref()) {
        (Some(lane), Some(model)) => {
            let lane = Lane::parse(lane).ok_or_else(|| bad("unknown lane"))?;
            let state = proxy.state.read().await;
            let evaluation =
                state.pool.evaluate_model_eligibility(lane, model, None, Utc::now()).ok_or_else(|| bad("invalid wire model"))?;
            return ok(json!({"model_eligibility":evaluation}));
        }
        (None, None) => {}
        _ => return Err(bad("lane and model must be provided together")),
    }
    let mut response = usage(&proxy.state.read().await.pool);
    response["hub_sync_configured"] = json!(proxy.hub_sync_configured);
    ok(response)
}

pub(crate) async fn refresh_selected(proxy: &Proxy, id: Option<&str>, family: Option<&str>) -> Result<Map<String, Value>, Failure> {
    if id.is_none() && family.is_none_or(|f| f == "antigravity") {
        proxy.refresh_local_commands(Duration::from_secs(15)).await;
    }
    let accounts = {
        let state = proxy.state.read().await;
        if let Some(id) = id {
            vec![state.pool.find(id).filter(|a| family.is_none_or(|f| a.lane.family() == f)).cloned().ok_or_else(missing)?]
        } else {
            state
                .pool
                .accounts
                .values()
                .filter(|a| !a.disabled && !a.api_key && family.is_none_or(|f| a.lane.family() == f))
                .cloned()
                .collect()
        }
    };
    let mut pending = stream::iter(accounts.into_iter().map(|account| async move {
        let result = if account.disabled || account.api_key {
            Err(crate::error_classify::probe_error("account does not support quota probes"))
        } else {
            proxy.refresh_usage(&account).await
        };
        (account.id, result)
    }))
    .buffer_unordered(4);
    let mut errors = Map::new();
    while let Some((id, result)) = pending.next().await {
        if let Err(e) = result
            && e.kind != contracts::providers::AccountErrorKind::QuotaProbeUnsupported
        {
            errors.insert(id, json!(e.summary()));
        }
    }
    if !errors.is_empty() {
        proxy.log("warn", "one or more quota probes failed").await;
    }
    Ok(errors)
}

async fn refresh(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Map<String, Value>>) -> Reply {
    let raw = Value::Object(raw);
    let errors = refresh_selected(&proxy, text(&raw, &["auth_index", "account_id"]), None).await?;
    let mut response = usage(&proxy.state.read().await.pool);
    response["errors"] = Value::Object(errors);
    ok(response)
}

#[derive(Default, Deserialize)]
struct Name {
    name: Option<String>,
}

async fn upload(State(proxy): State<Arc<Proxy>>, Query(query): Query<Name>, Json(raw): Json<Value>) -> Reply {
    let name = query.name.as_deref().ok_or_else(|| bad("credential file name required"))?;
    let accounts = parse_credentials(name, &raw, false).map_err(|_| bad("unsupported credential document"))?;
    let hydrate_google = accounts.iter().any(Account::needs_google_metadata);
    let credential_guard = proxy.oauth_refresh.lock().await;
    let mut state = proxy.state.write().await;
    if state.pool.accounts.values().any(|a| a.name == name && a.local) {
        return Err(Failure(StatusCode::CONFLICT, "discovered accounts cannot be overwritten"));
    }
    let path = proxy.config.auth_dir.join(name);
    let replaced = state
        .pool
        .accounts
        .values()
        .filter(|old| old.name == name && !accounts.iter().any(|a| a.id == old.id && a.source_fingerprint == old.source_fingerprint))
        .map(|a| a.id.clone())
        .collect::<Vec<_>>();
    state
        .pool
        .remove_account_overrides(&proxy.config.state_dir, &replaced, || write_private(&path, &serde_json::to_vec_pretty(&raw)?, true))
        .map_err(|_| disk())?;
    let ids: BTreeSet<_> = accounts.iter().map(|a| a.id.as_str()).collect();
    state.pool.accounts.retain(|id, a| a.name != name || ids.contains(id.as_str()));
    state.pool.insert(accounts);
    state.pool.revision += 1;
    drop(state);
    drop(credential_guard);
    proxy.persist_cooldown_state().await;
    proxy.notify_hub_sync();
    if hydrate_google {
        tokio::spawn(async move { proxy.hydrate_google_metadata().await });
    }
    ok(json!({"status":"ok"}))
}

#[derive(Default, Deserialize)]
pub(crate) struct Changes {
    pub disabled: Option<bool>,
    pub priority: Option<i32>,
    pub weight: Option<u32>,
    pub note: Option<Value>,
    pub plan: Option<Value>,
    pub tier_multiplier: Option<Value>,
}

impl Changes {
    fn edits_auth_fields(&self) -> bool {
        self.disabled.is_some() || self.priority.is_some() || self.weight.is_some() || self.note.is_some()
    }
}

fn scrub_saved_label(field: Option<&mut Value>, pool: &Pool, accounts: &[Account]) {
    let Some(field) = field else { return };
    let Some(raw) = field.as_str() else { return };
    let previous = accounts.iter().filter_map(|a| pool.accounts.get(&a.id));
    let mut known = previous.clone().filter_map(|a| a.known_safe_label(raw));
    let label = known.next().unwrap_or(raw);
    // A shared label may have different redactions for different keys.
    let label = if known.any(|other| other != label) { "[redacted]" } else { label };
    let secrets: Vec<_> = accounts.iter().chain(previous).flat_map(Account::current_secrets).collect();
    *field = json!(crate::error_classify::scrub_label(label, &secrets));
}

// Callers editing credential fields hold oauth_refresh before taking the pool write lock.
pub(crate) fn change(proxy: &Proxy, pool: &mut Pool, id: &str, changes: Changes) -> Result<(), Failure> {
    let found = pool.find(id).or_else(|| pool.accounts.values().find(|a| a.name == id)).cloned().ok_or_else(missing)?;
    let edits_auth = changes.edits_auth_fields();
    let note = match changes.note.as_ref() {
        Some(Value::String(s)) if s.len() <= 4096 => Some(Some(s.clone())),
        Some(Value::Null) => Some(None),
        Some(_) => return Err(bad("invalid credential note")),
        None => None,
    };
    let plan = match changes.plan {
        Some(Value::String(s)) if s.trim().chars().count() <= 128 => {
            let s = s.trim();
            Some((!s.is_empty()).then(|| s.to_owned()))
        }
        Some(Value::Null) => Some(None),
        Some(_) => return Err(bad("invalid credential plan")),
        None => None,
    };
    let tier_multiplier = match changes.tier_multiplier {
        Some(Value::Null) => Some(None),
        Some(Value::Number(n)) => {
            Some(Some(n.as_f64().filter(|n| n.is_finite() && *n > 0.0).ok_or_else(|| bad("invalid credential tier multiplier"))?))
        }
        Some(_) => return Err(bad("invalid credential tier multiplier")),
        None => None,
    };
    if changes.disabled.is_none()
        && changes.priority.is_none()
        && changes.weight.is_none()
        && note.is_none()
        && plan.is_none()
        && tier_multiplier.is_none()
    {
        return Err(bad("no credential changes supplied"));
    }
    let managed = if !found.local && edits_auth {
        if !filename(&found.name) {
            return Err(bad("invalid file name"));
        }
        let path = proxy.config.auth_dir.join(&found.name);
        let original = read_text(&path).map_err(|_| disk())?.ok_or_else(missing)?;
        let mut saved = serde_json::from_str::<Map<String, Value>>(&original).map_err(|_| disk())?;
        let key_index = found
            .id
            .strip_prefix(&found.name)
            .and_then(|suffix| suffix.strip_prefix('#'))
            .and_then(|n| n.parse::<usize>().ok())
            .and_then(|n| n.checked_sub(1));
        let apply_fields = |target: &mut Map<String, Value>| {
            if let Some(v) = changes.disabled {
                target.insert("disabled".into(), json!(v));
                target.remove("enabled");
            }
            if let Some(v) = changes.priority {
                target.insert("priority".into(), json!(v));
            }
            if let Some(v) = changes.weight {
                target.insert("weight".into(), json!(v));
            }
            if let Some(v) = &note {
                target.insert("note".into(), json!(v));
            }
        };
        if let Some(idx) = key_index {
            let keys = if saved.get("api_key").and_then(Value::as_object).is_some_and(|o| o.get("keys").is_some_and(Value::is_array)) {
                saved.get_mut("api_key").and_then(Value::as_object_mut).and_then(|o| o.get_mut("keys")).and_then(Value::as_array_mut)
            } else {
                saved.get_mut("keys").and_then(Value::as_array_mut)
            };
            let entry = keys.and_then(|arr| arr.get_mut(idx)).ok_or_else(missing)?;
            if let Some(raw_key) = entry.as_str().map(str::to_owned) {
                *entry = json!({ "api_key": raw_key });
            }
            let obj = entry.as_object_mut().ok_or_else(disk)?;
            apply_fields(obj);
        } else {
            apply_fields(&mut saved);
        }
        let mut saved = Value::Object(saved);
        let accounts = parse_credentials(&found.name, &saved, false).map_err(|_| disk())?;
        scrub_saved_label(saved.get_mut("label"), pool, &accounts);
        scrub_saved_label(saved.get_mut("name"), pool, &accounts);
        let group = if saved["api_key"].is_object() {
            let group = &mut saved["api_key"];
            scrub_saved_label(group.get_mut("label"), pool, &accounts);
            group
        } else {
            &mut saved
        };
        if let Some(keys) = group.get_mut("keys").and_then(Value::as_array_mut) {
            for (key, account) in keys.iter_mut().zip(&accounts) {
                scrub_saved_label(key.get_mut("label"), pool, std::slice::from_ref(account));
            }
        }
        Some((path, serde_json::to_vec_pretty(&saved).map_err(|_| disk())?, accounts))
    } else {
        None
    };
    // Publish auth metadata as soon as it is durable, even if the separate capacity write fails.
    let clean_managed_metadata = managed.is_some();
    if let Some((path, saved, accounts)) = managed {
        write_private(&path, &saved, true).map_err(|_| disk())?;
        pool.insert(accounts);
        pool.revision += 1;
    }
    pool.save_account_override(
        &proxy.config.state_dir,
        &found.id,
        AccountOverride {
            disabled: changes.disabled,
            priority: changes.priority,
            weight: changes.weight,
            note,
            plan,
            tier_multiplier,
            ..Default::default()
        },
        clean_managed_metadata,
    )
    .map_err(|_| disk())?;
    if !clean_managed_metadata {
        pool.revision += 1;
    }
    Ok(())
}

async fn edit_credential(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Map<String, Value>>) -> Reply {
    let raw = Value::Object(raw);
    let id = text(&raw, &["name", "auth_index", "account_id"]).ok_or_else(|| bad("credential name required"))?;
    let mut changes: Changes = serde_json::from_value(raw.clone()).map_err(|_| bad("invalid credential fields"))?;
    changes.note = raw.get("note").cloned();
    changes.plan = raw.get("plan").cloned();
    changes.tier_multiplier = raw.get("tier_multiplier").cloned();
    let _credential_guard = if changes.edits_auth_fields() { Some(proxy.oauth_refresh.lock().await) } else { None };
    let mut state = proxy.state.write().await;
    change(&proxy, &mut state.pool, id, changes)?;
    ok(json!({"status":"ok"}))
}

async fn delete_credential(State(proxy): State<Arc<Proxy>>, Query(query): Query<Name>) -> Reply {
    let name = query.name.as_deref().filter(|name| filename(name)).ok_or_else(|| bad("valid file name required"))?;
    {
        let _credential_guard = proxy.oauth_refresh.lock().await;
        let mut state = proxy.state.write().await;
        let mut candidates = state.pool.accounts.values().filter(|a| a.name == name).peekable();
        if candidates.peek().is_none() {
            return Err(missing());
        }
        if candidates.any(|a| a.local) {
            return Err(Failure(StatusCode::CONFLICT, "discovered credentials are read-only"));
        }
        let ids = state.pool.accounts.values().filter(|a| a.name == name).map(|a| a.id.clone()).collect::<Vec<_>>();
        state
            .pool
            .remove_account_overrides(&proxy.config.state_dir, &ids, || Ok(fs::remove_file(proxy.config.auth_dir.join(name))?))
            .map_err(|_| disk())?;
        state.pool.accounts.retain(|_, a| a.name != name);
        state.pool.revision += 1;
    }
    proxy.persist_cooldown_state().await;
    ok(json!({"status":"ok"}))
}

async fn renew_credentials(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Map<String, Value>>) -> Reply {
    let raw = Value::Object(raw);
    let name = text(&raw, &["name"]);
    if name.is_none() && raw["all"] == true {
        proxy.refresh_local_commands(Duration::from_secs(15)).await;
    }
    let accounts = {
        let state = proxy.state.read().await;
        state.pool.accounts.values().filter(|a| !a.disabled && name.is_none_or(|name| a.name == name)).cloned().collect::<Vec<_>>()
    };
    if name.is_some() && accounts.is_empty() {
        return Err(missing());
    }
    if name.is_none() && raw["all"] != true {
        return Err(bad("name or all required"));
    }
    let mut results = Vec::new();
    for account in accounts {
        let renewed = proxy.token(&account, true).await.is_ok();
        if name.is_some() && !renewed {
            return Err(upstream());
        }
        results.push(json!({"id":account.name,"success":renewed,"error":(!renewed).then_some("credential renewal failed")}));
    }
    ok(json!({"results":results}))
}

async fn models(State(proxy): State<Arc<Proxy>>, Query(query): Query<Name>) -> Reply {
    let mut seen = BTreeSet::new();
    let (mut models, upstreams) = {
        let state = proxy.state.read().await;
        let accounts = state.pool.accounts.values().filter(|a| query.name.as_deref().is_none_or(|name| a.name == name)).collect::<Vec<_>>();
        if query.name.is_some() && accounts.is_empty() {
            return Err(missing());
        }
        let models = accounts
            .iter()
            .flat_map(|a| a.models.iter())
            .map(|id| (id.clone(), json!({"id":id})))
            .collect::<std::collections::BTreeMap<_, _>>();
        let upstreams = accounts
            .into_iter()
            .filter(|a| !a.disabled && a.models.is_empty() && seen.insert((a.lane, a.api_key, a.base.clone())))
            .take(8)
            .cloned()
            .collect::<Vec<_>>();
        (models, upstreams)
    };
    let had_upstreams = !upstreams.is_empty();
    let mut succeeded = false;
    let mut probes = stream::iter(upstreams.into_iter().map(|account| {
        let proxy = proxy.clone();
        async move { probe_models(&proxy, &account).await }
    }))
    .buffer_unordered(4);
    while let Some(result) = probes.next().await {
        if let Ok(rows) = result {
            succeeded = true;
            models.extend(rows);
        }
    }
    if had_upstreams && !succeeded && models.is_empty() {
        return Err(upstream());
    }
    ok(json!({"models":models.into_values().collect::<Vec<_>>()}))
}

async fn probe_models(proxy: &Proxy, account: &Account) -> Result<Vec<(String, Value)>, Failure> {
    let account = proxy.token_account(account, false).await.map_err(|_| upstream())?;
    let token = account.access.clone();
    let account = &account;
    let path = match account.lane {
        Lane::Codex if !account.api_key => "/backend-api/codex/models",
        Lane::Google if account.api_key => "/v1beta/models",
        Lane::Google => "/v1internal:fetchAvailableModels",
        _ => "/v1/models",
    };
    let url = crate::router::upstream(proxy, account, &axum::http::Uri::from_static(path)).ok_or_else(upstream)?;
    let request = match account.lane {
        Lane::Claude => {
            let request = proxy.http.get(url).header("anthropic-version", "2023-06-01");
            if account.api_key { request.header("x-api-key", token) } else { proxy.claude_oauth(request, &token) }
        }
        Lane::Google if account.api_key => proxy.http.get(url).header("x-goog-api-key", token),
        Lane::Google => proxy
            .http
            .post(url)
            .bearer_auth(token)
            .json(&account.project.as_ref().map_or_else(|| json!({}), |project| json!({"project":project}))),
        Lane::Codex => openai_account(proxy.http.get(url).bearer_auth(token), account),
        Lane::Go => proxy.http.get(url).bearer_auth(token),
    };
    let raw = provider_json(request).await.map_err(|_| upstream())?;
    let model = |id: &str, row: &Value| {
        let mut public = json!({"id":id});
        for (field, keys) in [("display_name", ["display_name", "displayName"]), ("owned_by", ["owned_by", "modelProvider"])] {
            if let Some(value) = text(row, &keys) {
                public[field] = json!(value);
            }
        }
        if let Some(at) = row["created"].as_u64().filter(|n| *n > 0) {
            public["created"] = json!(at);
        }
        (id.to_owned(), public)
    };
    Ok(raw["data"]
        .as_array()
        .or_else(|| raw["models"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| Some(model(text(row, &["id", "slug", "name"])?, row)))
        .chain(raw["models"].as_object().into_iter().flatten().map(|(id, row)| model(id, row)))
        .collect())
}

async fn select(proxy: &Proxy, raw: &Value) -> Result<Account, Failure> {
    let id = text(raw, &["auth_index", "account_id"]).ok_or_else(|| bad("auth_index required"))?;
    proxy.state.read().await.pool.find(id).filter(|a| !a.disabled).cloned().ok_or_else(missing)
}

#[derive(Deserialize)]
struct ApiCall {
    #[serde(alias = "authIndex", alias = "Auth_Index")]
    auth_index: String,
    method: String,
    url: String,
    #[serde(default, alias = "headers")]
    header: std::collections::BTreeMap<String, String>,
    data: Option<String>,
}

pub(crate) fn allowed_call(account: &Account, url: &reqwest::Url, method: &str) -> bool {
    !account.api_key && allowed_lane(url, method, ManagementScope::Full) == Some(account.lane)
}

fn allowed_lane(url: &reqwest::Url, method: &str, scope: ManagementScope) -> Option<Lane> {
    if url.scheme() != "https"
        || url.port().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    match (url.host_str(), method, url.path()) {
        (Some("api.anthropic.com"), "GET", "/api/oauth/usage") => Some(Lane::Claude),
        (Some("chatgpt.com"), "GET", "/backend-api/wham/usage" | "/backend-api/wham/rate-limit-reset-credits") => Some(Lane::Codex),
        (Some("chatgpt.com"), "POST", "/backend-api/wham/rate-limit-reset-credits/consume") if scope == ManagementScope::Full => {
            Some(Lane::Codex)
        }
        (
            Some("daily-cloudcode-pa.googleapis.com" | "cloudcode-pa.googleapis.com"),
            "POST",
            "/v1internal:retrieveUserQuotaSummary" | "/v1internal:fetchAvailableModels",
        ) => Some(Lane::Google),
        (Some("opencode.ai"), "GET", "/zen/go/v1/usage") => Some(Lane::Go),
        _ => None,
    }
}

async fn api_call(
    State(proxy): State<Arc<Proxy>>,
    axum::Extension(scope): axum::Extension<ManagementScope>,
    Json(call): Json<ApiCall>,
) -> Reply {
    let url = reqwest::Url::parse(&call.url);
    let method = call.method.to_ascii_uppercase();
    let usage_lane = if scope == ManagementScope::UsageReadOnly {
        Some(url.as_ref().ok().and_then(|url| allowed_lane(url, &method, scope)).ok_or_else(usage_read_only)?)
    } else {
        None
    };
    let account = select(&proxy, &json!({"auth_index":call.auth_index})).await?;
    if usage_lane.is_some_and(|lane| account.api_key || account.lane != lane) {
        return Err(usage_read_only());
    }
    let url = url.map_err(|_| bad("invalid upstream URL"))?;
    // The offline verifier uses canonical provider URLs to exercise the allowlist before remapping to loopback.
    let url = if let (ManagementScope::UsageReadOnly, Some(mock)) = (scope, &proxy.management_mock) {
        mock.join(url.path()).map_err(|_| upstream())?
    } else {
        url
    };
    let mock = proxy.management_mock.as_ref().is_some_and(|m| {
        m.origin() == url.origin()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && matches!(method.as_str(), "GET" | "POST")
    });
    if scope == ManagementScope::Full && !mock && !allowed_call(&account, &url, &method) {
        return Err(Failure(StatusCode::FORBIDDEN, "upstream is not allowed for this credential"));
    }
    if call.data.as_ref().is_some_and(|s| s.len() > 64 * 1024 || method == "GET") || call.header.len() > 24 {
        return Err(bad("invalid API call body or headers"));
    }
    let http = if mock { proxy.http.clone() } else { management_http(&url).await? };
    let account = proxy.token_account(&account, false).await.map_err(|_| upstream())?;
    let request = |token: &str| {
        let mut request = http.request(if method == "POST" { reqwest::Method::POST } else { reqwest::Method::GET }, url.clone());
        for (key, value) in &call.header {
            let name = key.to_ascii_lowercase();
            if !matches!(
                name.as_str(),
                "authorization"
                    | "content-type"
                    | "accept"
                    | "anthropic-beta"
                    | "user-agent"
                    | "openai-beta"
                    | "originator"
                    | "chatgpt-account-id"
                    | "x-goog-api-client"
            ) || (value.contains("$TOKEN$") && name != "authorization")
            {
                return Err(bad("API call header is not allowed"));
            }
            if name == "authorization" && value != "Bearer $TOKEN$" {
                return Err(bad("invalid token substitution"));
            }
            if name == "chatgpt-account-id" && account.chatgpt_id.as_deref() != Some(value.as_str()) {
                return Err(bad("account header does not match credential"));
            }
            request = request.header(name, value.replace("$TOKEN$", token));
        }
        if let Some(data) = &call.data {
            request = request.body(data.clone());
        }
        Ok(request.timeout(Duration::from_secs(12)))
    };
    let mut response = request(&account.access)?.send().await.map_err(|_| upstream())?;
    let fresh = if response.status() == StatusCode::UNAUTHORIZED
        && !account.api_key
        && (matches!(account.lane, Lane::Claude | Lane::Codex) || (account.lane == Lane::Google && account.refresh.is_some()))
        && let Ok(fresh) = proxy.token_account(&account, true).await
        && fresh.access != account.access
    {
        response = request(&fresh.access)?.send().await.map_err(|_| upstream())?;
        Some(fresh)
    } else {
        None
    };
    let status = response.status().as_u16();
    let bytes = bounded_body(response, 2 * 1024 * 1024).await.map_err(|_| upstream())?;
    let mut body = String::from_utf8_lossy(&bytes).into_owned();
    for secret in [&proxy.config.management_key, &proxy.config.client_key, &proxy.config.usage_key]
        .into_iter()
        .chain(
            fresh.iter().chain([&account]).flat_map(|a| [&a.access].into_iter().chain(a.refresh.as_ref()).chain(a.google_secret.as_ref())),
        )
        .filter(|s| !s.is_empty())
    {
        body = body.replace(secret, "[REDACTED]");
    }
    ok(json!({"status_code":status,"body":body}))
}

async fn management_http(url: &reqwest::Url) -> Result<reqwest::Client, Failure> {
    let host = url.host_str().ok_or_else(|| bad("invalid upstream host"))?;
    let resolved = tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host, 443)))
        .await
        .map_err(|_| upstream())?
        .map_err(|_| upstream())?
        .collect::<Vec<_>>();
    if resolved.is_empty() || !resolved.iter().all(|address| public_ip(address.ip())) {
        return Err(Failure(StatusCode::FORBIDDEN, "upstream DNS did not resolve to public addresses"));
    }
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .resolve_to_addrs(host, &resolved)
        .build()
        .map_err(|_| upstream())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            a != 0
                && a < 224
                && !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && !matches!((a, b), (100, 64..=127) | (198, 18..=19))
                && (a, b, c) != (192, 0, 0)
        }
        IpAddr::V6(ip) => {
            let [a, b, c, d, ..] = ip.octets();
            (0x20..=0x3f).contains(&a) && (a, b, c, d) != (0x20, 0x01, 0x0d, 0xb8) && (a, b) != (0x20, 0x02)
        }
    }
}

fn clear_quota(pool: &mut Pool, account: &Account, manual: bool) -> usize {
    let Some(a) = pool.accounts.get_mut(&account.id).filter(|a| a.same_credential(account)) else { return 0 };
    a.clear_quota_cooldowns();
    a.grants.clear();
    a.next_grant = None;
    a.probe_generation = 0;
    if manual {
        a.clear_account_error();
        a.probe_failed = false;
    }
    let before = a.windows.len();
    a.windows.retain(|w| match w.kind {
        QuotaWindowKind::Session | QuotaWindowKind::Weekly => false,
        QuotaWindowKind::Daily | QuotaWindowKind::Monthly | QuotaWindowKind::ModelScoped => !manual,
        QuotaWindowKind::Custom => !manual || !enforces_quota(w),
        QuotaWindowKind::Spend => true,
    });
    a.google_fallback_keys.retain(|key| a.windows.iter().any(|w| &w.window_key == key));
    before - a.windows.len()
}

async fn reset_quota(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Value>) -> Reply {
    let account = select(&proxy, &raw).await?;
    let count = clear_quota(&mut proxy.state.write().await.pool, &account, true);
    proxy.persist_cooldown_state().await;
    proxy.notify_hub_sync();
    ok(json!({"status":"ok","windows_reset":count}))
}

fn redeem_id(account: &Account, credit: &str) -> Uuid {
    use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
    let mut hash = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    hash.update(&[0x6f, 0x1c, 0x2a, 0x9e, 0x2d, 0x4b, 0x4c, 0x1e, 0x9a, 0x7f, 0x3b, 0x8d, 0x5e, 0x0c, 0x1a, 0x42]);
    hash.update(format!("{}:{credit}", account.chatgpt_id.as_deref().unwrap_or(&account.id)).as_bytes());
    let hash = hash.finish();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.as_ref()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

async fn reset_credit(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Value>) -> Reply {
    let _guard = proxy.quota_actions.lock().await;
    let account = select(&proxy, &raw).await?;
    if account.api_key || !matches!(account.lane, Lane::Claude | Lane::Codex) {
        return Err(bad("upstream reset credit redemption is not supported for this account"));
    }
    let explicit = text(&raw, &["grant_id"]);
    if raw.get("grant_id").is_some() && (explicit.is_none() || account.lane == Lane::Codex) {
        return Err(bad("grant_id must be a nonempty string and only applies to Claude"));
    }
    proxy.refresh_usage(&account).await.map_err(|_| upstream())?;
    let account = select(&proxy, &raw).await?;
    let choice = explicit.or(account.next_grant.as_deref());
    let Some(grant) = account.grants.iter().find(|g| {
        choice == Some(g.grant_id.as_str()) && g.applicable && g.available_count > 0 && g.expires_at.is_none_or(|at| at > Utc::now())
    }) else {
        return ok(json!({"code":"no_credit","usage":usage(&proxy.state.read().await.pool)}));
    };
    let request = if account.lane == Lane::Claude {
        let organization = account
            .organization
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| bad("Claude login has no valid organization UUID"))?;
        let mut url = proxy.config.anthropic.clone();
        url.set_path(&format!("/api/organizations/{organization}/reset_rate_limits"));
        proxy
            .claude_oauth(proxy.http.post(url), &account.access)
            .json(&json!({"program":"cedar_ember","grant_id":grant.grant_id,"request_id":Uuid::new_v4()}))
    } else {
        openai_account(
            proxy.http.post(proxy.wham("/backend-api/wham/rate-limit-reset-credits/consume")).bearer_auth(&account.access),
            &account,
        )
        .json(&json!({"credit_id":grant.grant_id,"redeem_request_id":redeem_id(&account,&grant.grant_id)}))
    };
    let response = provider_json(request).await.map_err(|_| upstream())?;
    let code = if account.lane == Lane::Claude {
        match response["result"].as_str() {
            Some("reset") => "reset",
            Some("not_limited") => "nothing_to_reset",
            Some("already_used") => "already_redeemed",
            Some("ineligible") => "no_credit",
            _ => return Err(upstream()),
        }
    } else {
        response["code"]
            .as_str()
            .filter(|code| matches!(*code, "reset" | "already_redeemed" | "nothing_to_reset" | "no_credit"))
            .ok_or_else(upstream)?
    };
    let count = if account.lane == Lane::Codex && matches!(code, "reset" | "already_redeemed") {
        let count = clear_quota(&mut proxy.state.write().await.pool, &account, false);
        proxy.persist_cooldown_state().await;
        count
    } else {
        0
    };
    let refresh_error = proxy.refresh_usage(&account).await.err().map(|e| e.summary());
    if refresh_error.is_none() && account.lane == Lane::Claude && matches!(code, "reset" | "already_redeemed") {
        let mut state = proxy.state.write().await;
        let now = Utc::now();
        if let Some(current) = state
            .pool
            .accounts
            .get_mut(&account.id)
            .filter(|a| a.access == account.access && a.cooldown_until() == account.cooldown_until())
            && current.windows.iter().any(|w| grant.cleared_windows.contains(&w.window_key) && w.used_percent.is_some())
            && current.windows.iter().all(|w| !w.used_percent.is_some_and(|p| p >= 100.0 && w.resets_at.is_none_or(|t| t > now)))
        {
            current.cooldown = None;
            drop(state);
            proxy.persist_cooldown_state().await;
        }
    }
    ok(json!({"code":code,"windows_reset":count,"refresh_error":refresh_error,"usage":usage(&proxy.state.read().await.pool)}))
}

async fn start_window(State(proxy): State<Arc<Proxy>>, Json(raw): Json<Value>) -> Reply {
    let _guard = proxy.quota_actions.lock().await;
    let account = select(&proxy, &raw).await?;
    let model = text(&raw, &["model"]).filter(|model| model.len() <= 192).ok_or_else(|| bad("model required"))?;
    if account.api_key || !matches!(account.lane, Lane::Claude | Lane::Codex) {
        return Err(bad("rolling-window start unsupported for this account"));
    }
    let now = Utc::now();
    if account.windows.iter().any(|w| {
        w.kind == contracts::providers::QuotaWindowKind::Session
            && w.scope_model.is_none()
            && w.resets_at.is_some_and(|at| at > now)
            && (w.cadence == contracts::providers::ResetCadence::RollingOnUse || account.lane == Lane::Codex)
    }) {
        return Err(Failure(StatusCode::CONFLICT, "rolling window is already running"));
    }
    let account = proxy.token_account(&account, false).await.map_err(|_| upstream())?;
    let path = if account.lane == Lane::Claude { "/v1/messages" } else { "/backend-api/codex/responses" };
    let url = crate::router::upstream(&proxy, &account, &axum::http::Uri::from_static(path)).ok_or_else(upstream)?;
    let request = proxy.http.post(url).bearer_auth(&account.access);
    let request = if account.lane == Lane::Claude {
        request
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .json(&json!({"model":model,"max_tokens":1,"messages":[{"role":"user","content":"Hi"}]}))
    } else {
        openai_account(request, &account).json(&json!({"model":model,"stream":true,"store":false,"instructions":"Reply with OK.",
            "input":[{"role":"user","content":[{"type":"input_text","text":"Hi"}]}]}))
    };
    crate::router::run_window_start(proxy.clone(), &account, model, request).await.map_err(|_| upstream())?;
    let refresh_error = proxy.refresh_usage(&account).await.err().map(|e| e.summary());
    ok(json!({"usage":usage(&proxy.state.read().await.pool), "refresh_error":refresh_error}))
}

#[derive(Deserialize)]
struct Lease {
    enabled: bool,
    version: Option<u32>,
}

async fn capture_lease(State(proxy): State<Arc<Proxy>>) -> Reply {
    ok(json!({"enabled":proxy.state.read().await.captures.enabled(),"version":2,"leaseSeconds":120,"chunkBytes":crate::CAPTURE_BYTES}))
}

async fn set_capture_lease(State(proxy): State<Arc<Proxy>>, Json(lease): Json<Lease>) -> Reply {
    if lease.enabled && lease.version != Some(2) {
        return Err(Failure(StatusCode::CONFLICT, "capture lease version unsupported"));
    }
    let mut state = proxy.state.write().await;
    if !lease.enabled || !state.captures.enabled() {
        state.captures.lease_generation = state.captures.lease_generation.wrapping_add(1);
    }
    state.captures.lease = lease.enabled.then(|| Instant::now() + Duration::from_secs(120));
    ok(json!({"enabled":lease.enabled,"version":2,"leaseSeconds":120,"chunkBytes":crate::CAPTURE_BYTES}))
}

#[derive(Default, Deserialize)]
struct After {
    after: Option<u64>,
}

async fn captures(State(proxy): State<Arc<Proxy>>, Query(query): Query<After>) -> Reply {
    let state = proxy.state.read().await;
    let captures = &state.captures;
    let records: Vec<_> =
        captures.records.iter().filter(|r| r["sequence"].as_u64().is_some_and(|seq| seq > query.after.unwrap_or(0))).take(4).collect();
    ok(json!({"version":2,"epoch":captures.epoch,"dropped":captures.dropped,"records":records}))
}

#[derive(Deserialize)]
struct Ack {
    epoch: Uuid,
    sequence: u64,
}

async fn ack_captures(State(proxy): State<Arc<Proxy>>, Json(ack): Json<Ack>) -> Reply {
    let mut state = proxy.state.write().await;
    let captures = &mut state.captures;
    if ack.epoch != captures.epoch || ack.sequence > captures.sequence {
        return Err(bad("capture cursor changed"));
    }
    captures.records.retain(|r| r["sequence"].as_u64().is_some_and(|seq| seq > ack.sequence));
    ok(json!({"status":"ok"}))
}

async fn clear_captures(State(proxy): State<Arc<Proxy>>) -> Reply {
    let mut state = proxy.state.write().await;
    let lease = state.captures.lease;
    state.captures = crate::Captures { lease, ..Default::default() };
    ok(json!({"status":"ok"}))
}

pub(crate) fn client_status(proxy: &Proxy) -> Vec<contracts::providers::ClientWiringStatus> {
    Client::ALL.into_iter().map(|client| client.status(&proxy.config.home, &proxy.config.url, &proxy.config.client_key)).collect()
}

async fn clients(State(proxy): State<Arc<Proxy>>) -> Reply {
    ok(json!({"clients":client_status(&proxy)}))
}

pub(crate) async fn wire_client(proxy: Arc<Proxy>, id: &str, undo: bool) -> Result<(), Failure> {
    let client = Client::parse(id).map_err(|_| Failure(StatusCode::NOT_FOUND, "client not found"))?;
    tokio::task::spawn_blocking(move || {
        client.wire(&proxy.config.home, (!undo).then_some((&proxy.config.url[..], &proxy.config.client_key[..])))
    })
    .await
    .map_err(|_| disk())?
    .map_err(|_| disk())
}

async fn apply_client(State(proxy): State<Arc<Proxy>>, Path(id): Path<String>) -> Reply {
    wire_client(proxy, &id, false).await?;
    ok(json!({"status":"ok"}))
}

async fn undo_client(State(proxy): State<Arc<Proxy>>, Path(id): Path<String>) -> Reply {
    wire_client(proxy, &id, true).await?;
    ok(json!({"status":"ok"}))
}

#[derive(Default, Deserialize)]
struct Limit {
    limit: Option<usize>,
}

async fn logs(State(proxy): State<Arc<Proxy>>, Query(query): Query<Limit>) -> Reply {
    let state = proxy.state.read().await;
    let logs = &state.logs;
    let tail = logs.iter().skip(logs.len().saturating_sub(query.limit.unwrap_or(100)));
    ok(json!({"lines":tail.clone().map(|l| format!("[{}] [proxy] [{}] {}",l.at.to_rfc3339(),l.level,l.message)).collect::<Vec<_>>(),
        "entries":tail.collect::<Vec<_>>() }))
}

async fn shutdown(State(proxy): State<Arc<Proxy>>, req: Request) -> Response {
    if !req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map_or(proxy.config.bind.ip().is_loopback(), |info| info.0.ip().is_loopback())
    {
        return Failure(StatusCode::FORBIDDEN, "shutdown requires a loopback connection").into_response();
    }
    proxy.shutdown.notify_one();
    (StatusCode::ACCEPTED, Json(json!({"status":"shutting down"}))).into_response()
}
