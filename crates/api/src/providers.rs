//! HTTP API routes and background collector/retention loop for generic AI
//! model providers, multi-account quota tracking, activity analytics, routing,
//! capture policy and exports.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use contracts::providers::*;
use domain::{Actor, Hub};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{ApiErr, AppState, Caller, R, err};

pub fn routes() -> Router<AppState> {
    Router::new()
        // Providers, accounts, quota, models, key groups, routing, logs & settings
        .route("/providers", get(list_providers))
        .route("/providers/settings", get(get_settings).put(set_settings))
        .route("/providers/operations/claim", post(claim_operations))
        .route("/providers/operations/{op_id}", get(get_operation))
        .route("/providers/operations/{op_id}/receipt", post(report_operation_receipt))
        .route("/providers/{id}", get(get_provider))
        .route("/providers/{id}/accounts", get(list_accounts))
        .route("/providers/{id}/quota", get(quota_report))
        .route("/providers/{id}/models", get(list_models))
        .route("/providers/{id}/key-groups", get(list_key_groups))
        .route("/providers/{id}/routing", get(get_routing))
        .route("/providers/{id}/logs", get(get_logs))
        .route("/providers/{id}/operations", post(submit_operation))
        .route("/providers/{id}/observations", post(ingest_observations))
        // Activity: all 7 Spillway tabs + ingest + capture + exports + ClickHouse sink
        .route("/activity/ingest", post(ingest_activity))
        .route("/activity/overview", get(activity_overview))
        .route("/activity/compare", get(activity_compare))
        .route("/activity/requests", get(activity_requests))
        .route("/activity/requests/{id}", get(activity_request_detail))
        .route("/activity/requests/{id}/capture", get(activity_request_capture))
        .route("/activity/traces", get(activity_traces))
        .route("/activity/traces/{session_id}", get(activity_trace_detail))
        .route("/activity/drift", get(activity_drift))
        .route("/activity/totals", get(activity_totals))
        .route("/activity/thinking", get(activity_thinking))
        .route("/activity/evals", get(list_evals).post(create_eval))
        .route("/activity/evals/export", get(export_evals))
        .route("/activity/evals/import", post(import_evals))
        .route("/activity/evals/{id}", put(update_eval).delete(delete_eval))
        .route("/activity/evals/{id}/replay", post(replay_eval))
        .route("/activity/capture-policy", get(get_capture_policy).put(set_capture_policy))
        .route("/activity/clear", post(clear_data))
        .route("/activity/exports/diagnostic-preview", post(preview_diagnostic_bundle))
        .route("/activity/exports/portable-archive", post(export_portable_archive))
        .route("/activity/sinks/clickhouse", get(get_clickhouse).put(set_clickhouse))
}

async fn list_providers(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<ProviderSummary>> {
    domain::providers::list_providers(&s.hub, &a).await.map(Json).map_err(err(&a))
}

async fn get_provider(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<String>) -> R<ProviderSummary> {
    domain::providers::get_provider(&s.hub, &a, &id).await.map(Json).map_err(err(&a))
}

async fn list_accounts(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<String>) -> R<Vec<ModelAccount>> {
    domain::providers::list_accounts(&s.hub, &a, &id).await.map(Json).map_err(err(&a))
}

async fn quota_report(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<String>,
    Query(q): Query<QuotaQuery>,
) -> R<ProviderQuotaReport> {
    domain::providers::quota_report(&s.hub, &a, &id, q).await.map(Json).map_err(err(&a))
}

async fn list_models(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<String>) -> R<Vec<DiscoveredModel>> {
    domain::providers::list_models(&s.hub, &a, &id).await.map(Json).map_err(err(&a))
}

async fn list_key_groups(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<String>) -> R<Vec<KeyGroup>> {
    domain::providers::list_key_groups(&s.hub, &a, &id).await.map(Json).map_err(err(&a))
}

async fn get_routing(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<String>) -> R<RoutingView> {
    domain::providers::get_routing(&s.hub, &a, &id).await.map(Json).map_err(err(&a))
}

async fn get_logs(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<String>) -> R<Vec<ProxyLogEntry>> {
    domain::providers::get_logs(&s.hub, &a, &id).await.map(Json).map_err(err(&a))
}

async fn submit_operation(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<String>,
    Json(body): Json<SubmitOperationIn>,
) -> R<ProviderOperation> {
    domain::providers::submit_operation(&s.hub, &a, &id, body).await.map(Json).map_err(err(&a))
}

async fn get_operation(State(s): State<AppState>, Caller(a): Caller, Path(op_id): Path<Uuid>) -> R<ProviderOperation> {
    domain::providers::get_operation(&s.hub, &a, op_id).await.map(Json).map_err(err(&a))
}

async fn claim_operations(State(s): State<AppState>, Caller(a): Caller, Json(body): Json<ClaimOperationsIn>) -> R<Vec<ProviderOperation>> {
    domain::providers::claim_operations(&s.hub, &a, body).await.map(Json).map_err(err(&a))
}

async fn report_operation_receipt(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(op_id): Path<Uuid>,
    Json(body): Json<OperationReceiptIn>,
) -> R<ProviderOperation> {
    domain::providers::report_operation_receipt(&s.hub, &a, op_id, body).await.map(Json).map_err(err(&a))
}

async fn ingest_observations(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<String>,
    Json(batch): Json<ObservationBatch>,
) -> R<ObservationAck> {
    domain::providers::ingest_observations(&s.hub, &a, &id, batch).await.map(Json).map_err(err(&a))
}

async fn get_settings(State(s): State<AppState>, Caller(a): Caller) -> R<ProviderPanelSettings> {
    domain::providers::get_settings(&s.hub, &a).await.map(Json).map_err(err(&a))
}

async fn set_settings(
    State(s): State<AppState>,
    Caller(a): Caller,
    Json(settings): Json<ProviderPanelSettings>,
) -> R<ProviderPanelSettings> {
    domain::providers::set_settings(&s.hub, &a, settings).await.map(Json).map_err(err(&a))
}

// ── Activity handlers ───────────────────────────────────────────────────────

async fn ingest_activity(State(s): State<AppState>, Caller(a): Caller, Json(batch): Json<ActivityIngestBatch>) -> R<ActivityIngestAck> {
    domain::providers::activity::ingest_activity(&s.hub, &a, batch).await.map(Json).map_err(err(&a))
}

async fn activity_overview(State(s): State<AppState>, Caller(a): Caller, Query(scope): Query<ActivityScope>) -> R<ActivityOverview> {
    domain::providers::activity::overview(&s.hub, &a, scope).await.map(Json).map_err(err(&a))
}

async fn activity_compare(State(s): State<AppState>, Caller(a): Caller, Query(scope): Query<ActivityScope>) -> R<CompareReport> {
    domain::providers::activity::compare(&s.hub, &a, scope).await.map(Json).map_err(err(&a))
}

async fn activity_requests(State(s): State<AppState>, Caller(a): Caller, Query(scope): Query<ActivityScope>) -> R<RequestPage> {
    domain::providers::activity::requests(&s.hub, &a, scope).await.map(Json).map_err(err(&a))
}

async fn activity_request_detail(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<RequestDetail> {
    domain::providers::activity::request_detail(&s.hub, &a, id).await.map(Json).map_err(err(&a))
}

async fn activity_request_capture(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<CapturedBodyView> {
    domain::providers::activity::request_capture(&s.hub, &a, id).await.map(Json).map_err(err(&a))
}

async fn activity_traces(State(s): State<AppState>, Caller(a): Caller, Query(scope): Query<ActivityScope>) -> R<TracePage> {
    domain::providers::activity::traces(&s.hub, &a, scope).await.map(Json).map_err(err(&a))
}

async fn activity_trace_detail(State(s): State<AppState>, Caller(a): Caller, Path(session_id): Path<String>) -> R<TraceDetail> {
    domain::providers::activity::trace_detail(&s.hub, &a, &session_id).await.map(Json).map_err(err(&a))
}

async fn activity_drift(State(s): State<AppState>, Caller(a): Caller, Query(scope): Query<ActivityScope>) -> R<DriftReport> {
    domain::providers::activity::drift(&s.hub, &a, scope).await.map(Json).map_err(err(&a))
}

#[derive(Deserialize)]
struct TotalsQuery {
    #[serde(default)]
    granularity: TotalsGranularity,
    #[serde(flatten)]
    scope: ActivityScope,
}

async fn activity_totals(State(s): State<AppState>, Caller(a): Caller, Query(q): Query<TotalsQuery>) -> R<TotalsReport> {
    domain::providers::activity::totals(&s.hub, &a, q.granularity, q.scope).await.map(Json).map_err(err(&a))
}

async fn activity_thinking(State(s): State<AppState>, Caller(a): Caller, Query(scope): Query<ActivityScope>) -> R<ThinkingReport> {
    domain::providers::activity::thinking(&s.hub, &a, scope).await.map(Json).map_err(err(&a))
}

async fn list_evals(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<EvalCase>> {
    domain::providers::activity::list_evals(&s.hub, &a).await.map(Json).map_err(err(&a))
}

async fn create_eval(State(s): State<AppState>, Caller(a): Caller, Json(body): Json<EvalCaseIn>) -> R<EvalCase> {
    domain::providers::activity::create_eval(&s.hub, &a, body).await.map(Json).map_err(err(&a))
}

async fn update_eval(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(body): Json<EvalCaseIn>) -> R<EvalCase> {
    domain::providers::activity::update_eval(&s.hub, &a, id, body).await.map(Json).map_err(err(&a))
}

async fn delete_eval(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiErr> {
    domain::providers::activity::delete_eval(&s.hub, &a, id).await.map_err(err(&a))?;
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize, Default)]
struct ReplayIn {
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    model_override: Option<String>,
    #[serde(default)]
    effort_override: Option<String>,
}

async fn replay_eval(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<ReplayIn>,
) -> R<EvalReplayResult> {
    domain::providers::activity::replay_eval(&s.hub, &a, id, body.account_id, body.model_override, body.effort_override)
        .await
        .map(Json)
        .map_err(err(&a))
}

#[derive(Deserialize, Default)]
struct ExportEvalQuery {
    #[serde(default)]
    include_bodies: bool,
}

async fn export_evals(State(s): State<AppState>, Caller(a): Caller, Query(q): Query<ExportEvalQuery>) -> R<EvalExportBundle> {
    domain::providers::activity::export_evals(&s.hub, &a, q.include_bodies).await.map(Json).map_err(err(&a))
}

async fn import_evals(State(s): State<AppState>, Caller(a): Caller, Json(bundle): Json<EvalExportBundle>) -> R<Vec<EvalCase>> {
    domain::providers::activity::import_evals(&s.hub, &a, bundle).await.map(Json).map_err(err(&a))
}

async fn get_capture_policy(State(s): State<AppState>, Caller(a): Caller) -> R<CapturePolicy> {
    domain::providers::capture::get_capture_policy(&s.hub, &a).await.map(Json).map_err(err(&a))
}

async fn set_capture_policy(State(s): State<AppState>, Caller(a): Caller, Json(policy): Json<CapturePolicy>) -> R<CapturePolicy> {
    domain::providers::capture::set_capture_policy(&s.hub, &a, policy).await.map(Json).map_err(err(&a))
}

async fn clear_data(State(s): State<AppState>, Caller(a): Caller, Json(body): Json<ClearDataIn>) -> R<ClearDataOut> {
    domain::providers::capture::clear_data(&s.hub, &a, body).await.map(Json).map_err(err(&a))
}

async fn preview_diagnostic_bundle(
    State(s): State<AppState>,
    Caller(a): Caller,
    Json(body): Json<DiagnosticBundlePreviewIn>,
) -> R<DiagnosticBundlePreview> {
    domain::providers::capture::preview_diagnostic_bundle(&s.hub, &a, body).await.map(Json).map_err(err(&a))
}

async fn export_portable_archive(
    State(s): State<AppState>,
    Caller(a): Caller,
    Json(body): Json<PortableArchiveRequest>,
) -> R<PortableArchiveExport> {
    domain::providers::capture::export_portable_archive(&s.hub, &a, body).await.map(Json).map_err(err(&a))
}

async fn get_clickhouse(State(s): State<AppState>, Caller(a): Caller) -> R<ClickHouseStatus> {
    domain::providers::capture::get_clickhouse_status(&s.hub, &a).await.map(Json).map_err(err(&a))
}

async fn set_clickhouse(State(s): State<AppState>, Caller(a): Caller, Json(config): Json<ClickHouseConfig>) -> R<ClickHouseStatus> {
    domain::providers::capture::set_clickhouse_config(&s.hub, &a, config).await.map(Json).map_err(err(&a))
}

/// Background loop polling server-side `ModelProvider::collect()` implementations
/// and enforcing capture/detail retention windows.
pub async fn run_loop(hub: Hub) {
    let mut poll_tick = tokio::time::interval(Duration::from_secs(60));
    let mut retention_counter = 0u32;
    let system_actor = Actor {
        principal: contracts::Principal { id: Uuid::nil(), name: "provider-poller".into(), role: contracts::Role::Operator },
        correlation_id: None,
    };
    loop {
        poll_tick.tick().await;
        for (id, provider) in hub.providers.iter() {
            match provider.collect().await {
                Ok(Some(batch)) => {
                    if let Err(error) = domain::providers::ingest_observations(&hub, &system_actor, id, batch).await {
                        tracing::warn!(provider = %id, %error, "provider observation ingest failed");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(provider = %id, %error, "provider collect failed");
                }
            }
        }
        retention_counter = retention_counter.wrapping_add(1);
        if retention_counter.is_multiple_of(15)
            && let Err(error) = domain::providers::capture::enforce_retention(&hub).await
        {
            tracing::warn!(%error, "provider capture retention failed");
        }
    }
}
