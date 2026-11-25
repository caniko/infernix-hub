//! HTTP API routes and background collector/retention loop for generic AI
//! model providers, multi-account quota tracking, activity analytics, routing,
//! capture policy and exports.

use std::{collections::BTreeMap, time::Duration};

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use contracts::Severity;
use contracts::providers::*;
use domain::notifications::{self, Raise};
use domain::ports::PortError;
use domain::providers::provider_notification_key;
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
    domain::providers::ingest_pushed_observations(&s.hub, &a, &id, batch).await.map(Json).map_err(err(&a))
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
    let mut failures = BTreeMap::new();
    let system_actor = Actor {
        principal: contracts::Principal { id: Uuid::nil(), name: "provider-poller".into(), role: contracts::Role::Operator },
        correlation_id: None,
    };
    loop {
        poll_tick.tick().await;
        for (id, provider) in hub.providers.iter() {
            poll_provider(&hub, &system_actor, id, provider.as_ref(), failures.entry(id).or_default()).await;
        }
        retention_counter = retention_counter.wrapping_add(1);
        if retention_counter.is_multiple_of(15) {
            crate::record_notification_sync(
                &hub,
                "provider-retention",
                domain::providers::capture::enforce_retention(&hub).await.map(|_| 0),
            );
        }
    }
}

async fn poll_provider(hub: &Hub, actor: &Actor, id: &str, provider: &dyn domain::ports::ModelProvider, streak: &mut u32) {
    let result = match provider.collect().await {
        Ok(Some(batch)) => domain::providers::ingest_observations(hub, actor, id, batch)
            .await
            .map(|_| ())
            .map_err(|error| PortError::Unreachable(format!("observation ingest failed: {error}"))),
        Ok(None) => return,
        Err(error) => Err(error),
    };
    if let Some(raises) = poller_raises(id, streak, result.as_ref().err()) {
        let source = poller_source(id);
        crate::record_notification_sync(hub, &source, notifications::sync(hub, &source, raises).await);
    }
    let source = format!("provider-poll:{id}");
    match result {
        Ok(()) => hub.record_sync_ok(&source),
        Err(error) => {
            let detail = sanitize_provider_error(&error.to_string());
            hub.record_sync_error(&source, &detail);
            tracing::error!(provider = %id, %detail, consecutive_failures = *streak, "provider poll failed");
        }
    }
}

fn poller_source(id: &str) -> String {
    format!("providers-poller:{id}")
}

/// Only a successful poll and ingest clear outstanding alerts.
fn poller_raises(id: &str, failures: &mut u32, error: Option<&PortError>) -> Option<Vec<Raise>> {
    let Some(error) = error else {
        *failures = 0;
        return Some(Vec::new());
    };
    *failures = failures.saturating_add(1);
    let detail = sanitize_provider_error(&error.to_string());
    let lower = detail.to_ascii_lowercase();
    let immediate = matches!(error, PortError::Rejected { status: 401 | 403, .. } | PortError::Unsupported(_))
        || [
            "unauthorized",
            "forbidden",
            "access denied",
            "authentication",
            "auth failed",
            "invalid credentials",
            "invalid api key",
            "credential file",
            "invalid config",
            "missing config",
            "misconfigured",
            "not configured",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        || lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|word| matches!(word, "401" | "403"));
    if *failures < 2 && !immediate {
        return None;
    }
    Some(vec![Raise {
        source: poller_source(id),
        dedupe_key: provider_notification_key(id, "poller:error"),
        severity: if *failures >= 5 { Severity::Critical } else { Severity::Warning },
        title: format!("{}: provider poller failed", sanitize_provider_error(id)),
        body: detail,
        subject_type: Some("model_provider".into()),
        subject_id: Some(id.into()),
        url: None,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poller_failures_include_ingestion_and_recovery_requires_a_saved_batch() {
        use domain::ports::{ModelProvider, PortResult, ProviderFacets};
        use domain::sqlx;
        struct Poller(ProviderDescriptor, Option<ObservationBatch>);

        #[async_trait::async_trait]
        impl ModelProvider for Poller {
            fn descriptor(&self) -> ProviderDescriptor {
                self.0.clone()
            }
            fn facets(&self) -> ProviderFacets<'_> {
                ProviderFacets::observation_only(Default::default())
            }
            async fn collect(&self) -> PortResult<Option<ObservationBatch>> {
                Ok(self.1.clone())
            }
        }

        let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
            eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
            return;
        };
        let admin = sqlx::PgPool::connect(&base).await.unwrap();
        let name = format!("t_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
        let mut url = reqwest::Url::parse(&base).unwrap();
        url.set_path(&format!("/{name}"));
        let mut settings = crate::bootstrap::Settings::fixture(url.as_str(), "127.0.0.1:0");
        settings.asset_dir = std::env::temp_dir().join(&name);
        settings.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
        settings.seed_file = Some(settings.registry_dir.join("hub.fixture.json"));
        let (state, _) = crate::bootstrap::build(&settings).await.unwrap();
        let hub = &state.hub;
        let actor = Actor {
            principal: contracts::Principal { id: Uuid::nil(), name: "provider-poller".into(), role: contracts::Role::Operator },
            correlation_id: None,
        };
        let fixture = integrations::providers::FullFixtureModelProvider::new("audit-poller", "Audit Poller", "test");
        let mut batch = fixture.collect().await.unwrap().unwrap();
        let good = Poller(fixture.descriptor(), Some(batch.clone()));
        batch.provider_id = "incorrect-provider-id".into();
        let bad = Poller(fixture.descriptor(), Some(batch));
        let noop = Poller(fixture.descriptor(), None);
        let open = || {
            sqlx::query_scalar::<_, bool>(
                "select exists (select 1 from notifications where source = 'providers-poller:audit-poller' and resolved_at is null)",
            )
            .fetch_one(&hub.db)
        };
        let mut streak = 0;
        for expected in 1..=2 {
            poll_provider(hub, &actor, "audit-poller", &bad, &mut streak).await;
            assert_eq!(streak, expected);
            assert!(hub.sync_failures().iter().any(|f| f.consecutive_failures == expected));
        }
        assert!(open().await.unwrap());
        poll_provider(hub, &actor, "audit-poller", &noop, &mut streak).await;
        assert!(open().await.unwrap());
        assert_eq!(streak, 2);
        poll_provider(hub, &actor, "audit-poller", &good, &mut streak).await;
        assert!(!open().await.unwrap());
        assert_eq!(streak, 0);
        assert!(hub.sync_failures().is_empty());
        sqlx::query("alter table notifications rename to broken_notifications").execute(&hub.db).await.unwrap();
        poll_provider(hub, &actor, "audit-poller", &bad, &mut streak).await;
        poll_provider(hub, &actor, "audit-poller", &bad, &mut streak).await;
        assert!(hub.sync_failures().iter().any(|f| f.source == "providers-poller:audit-poller"));
        sqlx::query("alter table broken_notifications rename to notifications").execute(&hub.db).await.unwrap();
        poll_provider(hub, &actor, "audit-poller", &good, &mut streak).await;
        assert!(hub.sync_failures().is_empty());
        let publishers: i64 = sqlx::query_scalar("select count(*) from model_provider_publishers where provider_id = 'audit-poller'")
            .fetch_one(&hub.db)
            .await
            .unwrap();
        assert_eq!(publishers, 0, "pull providers never register a push collector");
        hub.db.close().await;
        sqlx::query(&format!("drop database {name}")).execute(&admin).await.unwrap();
        let _ = std::fs::remove_dir_all(settings.asset_dir);
    }

    #[test]
    fn poller_alerts_escalate_and_reset_on_any_success() {
        let mut streak = 0;
        let unavailable = PortError::Unreachable("request failed".into());
        let mut step = |error| poller_raises("test", &mut streak, error);
        assert!(step(Some(&unavailable)).is_none());
        for count in 2..=6 {
            let raises = step(Some(&unavailable)).unwrap();
            assert_eq!(raises[0].severity, if count >= 5 { Severity::Critical } else { Severity::Warning });
            assert_eq!(raises[0].source, "providers-poller:test");
            assert_eq!(raises[0].dedupe_key, "provider:test:poller:error");
        }
        assert!(step(None).unwrap().is_empty());
        assert!(step(Some(&unavailable)).is_none());
    }

    #[test]
    fn auth_and_configuration_failures_alert_immediately_without_secrets() {
        for error in [
            PortError::Rejected { status: 401, message: "Authorization: bearer very-private".into() },
            PortError::Rejected { status: 401, message: "Authorization: basic very-private".into() },
            PortError::Rejected { status: 403, message: "https://example.com/path?token=very-private".into() },
            PortError::Unreachable("credential file is unreadable".into()),
            PortError::Unsupported(format!("invalid configuration: access_token=very-private {}", "a".repeat(600))),
        ] {
            let raises = poller_raises("test", &mut 0, Some(&error)).unwrap();
            assert_eq!(raises[0].severity, Severity::Warning);
            assert!(!raises[0].body.contains("very-private"));
            assert!(raises[0].body.len() < 400);
        }
    }
}
