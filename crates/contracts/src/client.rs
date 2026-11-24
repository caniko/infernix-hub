//! Typed HTTP client for the hub API, shared by the desktop UI and the CLI so
//! both go through the same server-side authorization and command handling.

use crate::*;
use serde::de::DeserializeOwned;

#[derive(Debug)]
pub enum ClientError {
    Transport(String),
    Api { status: u16, error: ApiError },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Transport(e) => write!(f, "hub unreachable: {e}"),
            ClientError::Api { status, error } => {
                write!(f, "{} ({status}): {}", error.code, error.message)
            }
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Clone)]
pub struct HubClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for HubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubClient").field("base", &self.base).field("token", &"<redacted>").finish()
    }
}

impl HubClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self { http: reqwest::Client::new(), base, token: token.into() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{API_VERSION}{path}", self.base)
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ClientError> {
        let resp = req
            .bearer_auth(&self.token)
            .header(CORRELATION_HEADER, uuid::Uuid::new_v4().to_string())
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            let error = serde_json::from_slice(&body).unwrap_or_else(|_| ApiError {
                code: "http_error".into(),
                message: String::from_utf8_lossy(&body).into_owned(),
                correlation_id: None,
            });
            return Err(ClientError::Api { status: status.as_u16(), error });
        }
        let body: &[u8] = if body.is_empty() { b"null" } else { &body };
        serde_json::from_slice(body).map_err(|e| ClientError::Transport(format!("decode: {e}")))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send(self.http.get(self.url(path))).await
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, ClientError> {
        self.send(self.http.post(self.url(path)).json(body)).await
    }

    pub async fn put<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, ClientError> {
        self.send(self.http.put(self.url(path)).json(body)).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send(self.http.delete(self.url(path))).await
    }

    /// Raw bytes (asset content) — returns `(mime, bytes)`.
    pub async fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>), ClientError> {
        let resp =
            self.http.get(self.url(path)).bearer_auth(&self.token).send().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ClientError::Api {
                status: status.as_u16(),
                error: ApiError { code: "http_error".into(), message: String::from_utf8_lossy(&bytes).into(), correlation_id: None },
            });
        }
        Ok((mime, bytes.to_vec()))
    }

    // Typed helpers for the main journey.
    pub async fn status(&self) -> Result<SystemStatus, ClientError> {
        self.get("/status").await
    }
    /// The fleet operations snapshot; read-only, produced by another tool.
    pub async fn fleet_status(&self) -> Result<fleet::FleetStatus, ClientError> {
        self.get("/fleet/status").await
    }
    pub async fn secrets(&self) -> Result<SecretInventoryView, ClientError> {
        self.get("/secrets").await
    }
    pub async fn today(&self) -> Result<Today, ClientError> {
        self.get("/today").await
    }
    pub async fn news(&self) -> Result<Vec<NewsItem>, ClientError> {
        self.get("/news").await
    }
    pub async fn personas(&self) -> Result<Vec<Persona>, ClientError> {
        self.get("/personas").await
    }
    pub async fn individual_look(&self, id: Uuid) -> Result<IndividualLook, ClientError> {
        self.get(&format!("/personas/{id}/look")).await
    }
    pub async fn pin_look_reference(&self, id: Uuid, body: &PinIn) -> Result<PinOut, ClientError> {
        self.post(&format!("/personas/{id}/look/reference"), body).await
    }
    pub async fn upload_look_image(&self, id: Uuid, mime: &str, attested: bool, bytes: Vec<u8>) -> Result<Asset, ClientError> {
        let mut req =
            self.http.post(self.url(&format!("/personas/{id}/look/uploads"))).header(reqwest::header::CONTENT_TYPE, mime).body(bytes);
        if attested {
            req = req.header("x-attest-not-real-person", "true");
        }
        self.send(req).await
    }
    pub async fn add_look_source(&self, id: Uuid, body: &LookSourceIn) -> Result<Asset, ClientError> {
        self.post(&format!("/personas/{id}/look/sources"), body).await
    }
    pub async fn request_look_pass(&self, id: Uuid, body: &LookPassIn) -> Result<LookPass, ClientError> {
        self.post(&format!("/personas/{id}/look/passes"), body).await
    }
    pub async fn projects(&self) -> Result<Vec<Project>, ClientError> {
        self.get("/projects").await
    }
    pub async fn project(&self, id: Uuid) -> Result<Project, ClientError> {
        self.get(&format!("/projects/{id}")).await
    }
    pub async fn create_project(&self, body: &ProjectIn) -> Result<Project, ClientError> {
        self.post("/projects", body).await
    }
    pub async fn update_project(&self, id: Uuid, body: &ProjectIn) -> Result<Project, ClientError> {
        self.put(&format!("/projects/{id}"), body).await
    }
    pub async fn put_project_member(&self, id: Uuid, persona: Uuid, body: &ProjectMemberIn) -> Result<Project, ClientError> {
        self.put(&format!("/projects/{id}/members/{persona}"), body).await
    }
    pub async fn remove_project_member(&self, id: Uuid, persona: Uuid) -> Result<Project, ClientError> {
        self.delete(&format!("/projects/{id}/members/{persona}")).await
    }
    pub async fn set_project_accounts(&self, id: Uuid, body: &ProjectAccountsIn) -> Result<Project, ClientError> {
        self.put(&format!("/projects/{id}/accounts"), body).await
    }
    pub async fn workflows(&self) -> Result<Vec<Workflow>, ClientError> {
        self.get("/workflows").await
    }
    pub async fn accounts(&self) -> Result<Vec<Account>, ClientError> {
        self.get("/accounts").await
    }
    pub async fn content(&self) -> Result<Vec<ContentItem>, ClientError> {
        self.get("/content").await
    }
    pub async fn content_detail(&self, id: Uuid) -> Result<ContentDetail, ClientError> {
        self.get(&format!("/content/{id}")).await
    }
    pub async fn create_brief(&self, body: &CreateBriefIn) -> Result<ContentItem, ClientError> {
        self.post("/content", body).await
    }
    pub async fn generate(&self, content: Uuid, body: &GenerateIn) -> Result<GenerationRequest, ClientError> {
        self.post(&format!("/content/{content}/generations"), body).await
    }
    pub async fn cancel_generation(&self, id: Uuid) -> Result<CancelOut, ClientError> {
        self.post(&format!("/generations/{id}/cancel"), &()).await
    }
    pub async fn select_asset(&self, id: Uuid, body: &SelectAssetIn) -> Result<Asset, ClientError> {
        self.post(&format!("/assets/{id}/select"), body).await
    }
    pub async fn freeze_release(&self, content: Uuid, body: &FreezeReleaseIn) -> Result<ReleaseRevision, ClientError> {
        self.post(&format!("/content/{content}/releases"), body).await
    }
    pub async fn request_review(&self, release: Uuid) -> Result<ReleaseRevision, ClientError> {
        self.post(&format!("/releases/{release}/request-review"), &()).await
    }
    pub async fn schedule(&self, release: Uuid, body: &ScheduleIn) -> Result<Vec<PublicationIntent>, ClientError> {
        self.post(&format!("/releases/{release}/schedule"), body).await
    }
    pub async fn cancel_intent(&self, intent: Uuid) -> Result<PublicationIntent, ClientError> {
        self.post(&format!("/intents/{intent}/cancel"), &()).await
    }
    pub async fn intents(&self) -> Result<Vec<PublicationIntent>, ClientError> {
        self.get("/intents").await
    }
    pub async fn relationships(&self) -> Result<Vec<Relationship>, ClientError> {
        self.get("/relationships").await
    }
    pub async fn relationship(&self, id: Uuid) -> Result<RelationshipDetail, ClientError> {
        self.get(&format!("/relationships/{id}")).await
    }
    pub async fn draft_reply(&self, rel: Uuid, body: &DraftReplyIn) -> Result<ReplyDraft, ClientError> {
        self.post(&format!("/relationships/{rel}/drafts"), body).await
    }
    /// Ask the assistant for a reply draft (operator only).
    pub async fn draft_assistant_reply(&self, rel: Uuid) -> Result<ReplyDraft, ClientError> {
        self.post(&format!("/relationships/{rel}/draft-reply"), &()).await
    }
    pub async fn decide_draft(&self, draft: Uuid, body: &DraftDecisionIn) -> Result<ReplyDraft, ClientError> {
        self.post(&format!("/drafts/{draft}/decision"), body).await
    }
    pub async fn takeover(&self, rel: Uuid) -> Result<Relationship, ClientError> {
        self.post(&format!("/relationships/{rel}/takeover"), &()).await
    }
    pub async fn release_to_agent(&self, rel: Uuid) -> Result<Relationship, ClientError> {
        self.post(&format!("/relationships/{rel}/release"), &()).await
    }
    pub async fn handoff(&self, rel: Uuid) -> Result<HandoffOut, ClientError> {
        self.post(&format!("/relationships/{rel}/handoff"), &()).await
    }
    pub async fn chat(&self, subject_type: &str, id: Uuid) -> Result<Vec<ChatMessage>, ClientError> {
        self.get(&format!("/chat/{subject_type}/{id}")).await
    }
    pub async fn send_chat(&self, subject_type: &str, id: Uuid, body: &ChatMessageIn) -> Result<Vec<ChatMessage>, ClientError> {
        self.post(&format!("/chat/{subject_type}/{id}"), body).await
    }
    pub async fn canon(&self, persona: Uuid) -> Result<Vec<CanonRevision>, ClientError> {
        self.get(&format!("/personas/{persona}/canon")).await
    }
    pub async fn set_profile(&self, persona: Uuid, body: &IndividualProfile) -> Result<Persona, ClientError> {
        self.put(&format!("/personas/{persona}/profile"), body).await
    }
    pub async fn draft_canon(&self, persona: Uuid, body: &CanonDraftIn) -> Result<CanonBody, ClientError> {
        self.post(&format!("/personas/{persona}/canon-draft"), body).await
    }
    pub async fn request_canon_comments(&self, revision: Uuid) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/canon/{revision}/request-comments"), &()).await
    }
    pub async fn comment_canon(&self, revision: Uuid, body: &CanonCommentIn) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/canon/{revision}/comments"), body).await
    }
    pub async fn canon_reviews(&self) -> Result<Vec<CanonRevision>, ClientError> {
        self.get("/canon/reviews").await
    }
    pub async fn propose_canon(&self, persona: Uuid, body: &CanonProposalIn) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/personas/{persona}/canon"), body).await
    }
    pub async fn decide_canon(&self, revision: Uuid, body: &CanonDecisionIn) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/canon/{revision}/decision"), body).await
    }
    pub async fn search(&self, q: &str) -> Result<Vec<SearchHit>, ClientError> {
        self.send(self.http.get(self.url("/search")).query(&[("q", q)])).await
    }
    pub async fn list_mail_accounts(&self) -> Result<Vec<MailAccount>, ClientError> {
        self.get("/mail/accounts").await
    }
    pub async fn ingest_mail_messages(&self, body: &MailIngestRequest) -> Result<MailIngestResult, ClientError> {
        self.post("/mail/ingest", body).await
    }
    pub async fn list_mail_messages(&self, query: &MailSearchQuery) -> Result<Vec<MailMessageSummary>, ClientError> {
        self.send(self.http.get(self.url("/mail/messages")).query(query)).await
    }
    pub async fn get_mail_message(&self, id: Uuid) -> Result<MailMessageDetail, ClientError> {
        self.get(&format!("/mail/messages/{id}")).await
    }
    pub async fn override_mail_triage(&self, id: Uuid, body: &MailTriageOverride) -> Result<MailMessageDetail, ClientError> {
        self.put(&format!("/mail/messages/{id}/triage"), body).await
    }
    pub async fn mail_bi_overview(&self) -> Result<MailBiOverview, ClientError> {
        self.get("/mail/bi/overview").await
    }
    pub async fn create_mail_bi_digest(
        &self,
        period_kind: &str,
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
    ) -> Result<MailBiDigest, ClientError> {
        self.post(
            "/mail/bi/digests",
            &serde_json::json!({ "period_kind": period_kind, "period_start": period_start, "period_end": period_end }),
        )
        .await
    }
    pub async fn list_mail_bi_digests(&self) -> Result<Vec<MailBiDigest>, ClientError> {
        self.get("/mail/bi/digests").await
    }
    pub async fn list_calendar_sources(&self) -> Result<Vec<CalendarSource>, ClientError> {
        self.get("/calendar/sources").await
    }
    pub async fn list_calendar_events(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<CalendarEvent>, ClientError> {
        self.send(self.http.get(self.url("/calendar/events")).query(&[("from", from), ("to", to)])).await
    }
    pub async fn calendar_agenda(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<CalendarAgendaResponse, ClientError> {
        self.send(self.http.get(self.url("/calendar/agenda")).query(&[("from", from), ("to", to)])).await
    }
    pub async fn assets(&self) -> Result<Vec<Asset>, ClientError> {
        self.get("/assets").await
    }
    pub async fn set_pause(&self, body: &PauseIn) -> Result<SystemStatus, ClientError> {
        self.post("/system/pause", body).await
    }
    // ── Opportunities (H1) ──────────────────────────────────────────────
    pub async fn opportunities(&self, state: Option<&str>, due_within_days: Option<i32>) -> Result<Vec<Opportunity>, ClientError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(state) = state {
            query.push(("state", state.to_string()));
        }
        if let Some(days) = due_within_days {
            query.push(("due_within_days", days.to_string()));
        }
        self.send(self.http.get(self.url("/opportunities")).query(&query)).await
    }
    pub async fn opportunity(&self, id: Uuid) -> Result<Opportunity, ClientError> {
        self.get(&format!("/opportunities/{id}")).await
    }
    pub async fn ingest_opportunity(&self, body: &OpportunityIn) -> Result<Opportunity, ClientError> {
        self.post("/ingest/opportunities", body).await
    }
    pub async fn decide_opportunity(&self, id: Uuid, body: &OpportunityDecisionIn) -> Result<Opportunity, ClientError> {
        self.post(&format!("/opportunities/{id}/decision"), body).await
    }
    // ── Renewals (H3) ───────────────────────────────────────────────────
    pub async fn renewals(&self) -> Result<Vec<Renewal>, ClientError> {
        self.get("/renewals").await
    }
    /// Operator-only: record a renewal; `due_at` wins over the stored cadence.
    pub async fn renewal_renewed(&self, key: &str, body: &RenewalRenewedIn) -> Result<Renewal, ClientError> {
        self.post(&format!("/renewals/{key}/renewed"), body).await
    }
    /// Operator or procedure: record one provider answer. Never a rotation.
    pub async fn observe_renewal(&self, key: &str, body: &ObserveRenewalIn) -> Result<Renewal, ClientError> {
        self.post(&format!("/renewals/{key}/observe"), body).await
    }
    // ── Notifications ───────────────────────────────────────────────────
    /// Rows newer than `after`; `wait` seconds holds the request open on the
    /// server until something changes. `all` includes acknowledged and resolved
    /// rows.
    /// `after: None` reads a snapshot of the current set; `Some(cursor)` reads
    /// the feed of changes past `cursor` (echo the returned `cursor` back).
    pub async fn notifications(&self, after: Option<i64>, wait: u64, all: bool) -> Result<NotificationPage, ClientError> {
        let mut query: Vec<(&str, String)> = vec![("state", if all { "all".into() } else { "open".into() })];
        if let Some(after) = after {
            query.push(("after", after.to_string()));
        }
        if wait > 0 {
            query.push(("wait", wait.to_string()));
        }
        self.send(self.http.get(self.url("/notifications")).query(&query)).await
    }
    pub async fn raise_notification(&self, body: &RaiseNotificationIn) -> Result<Notification, ClientError> {
        self.post("/notifications", body).await
    }
    pub async fn sync_notifications(&self, body: &SyncNotificationsIn) -> Result<SyncNotificationsOut, ClientError> {
        self.post("/notifications/sync", body).await
    }
    /// Acknowledge `ids` or `all`; returns the remaining open page.
    pub async fn ack_notifications(&self, body: &AckNotificationsIn) -> Result<NotificationPage, ClientError> {
        self.post("/notifications/ack", body).await
    }
    /// Hide one row until `until`.
    pub async fn snooze_notification(&self, id: Uuid, body: &SnoozeNotificationIn) -> Result<Notification, ClientError> {
        self.post(&format!("/notifications/{id}/snooze"), body).await
    }
    // ── Release approval (H2) ───────────────────────────────────────────
    pub async fn release(&self, id: Uuid) -> Result<ReleaseRevision, ClientError> {
        self.get(&format!("/releases/{id}")).await
    }
    /// Re-read the native approval and bind it to this exact revision.
    pub async fn link_approval(&self, release: Uuid, body: &LinkApprovalIn) -> Result<ReleaseRevision, ClientError> {
        self.post(&format!("/releases/{release}/link-approval"), body).await
    }

    // ── Model providers, quota, activity, routing & exports ─────────────
    pub async fn providers(&self) -> Result<Vec<providers::ProviderSummary>, ClientError> {
        self.get("/providers").await
    }
    pub async fn provider(&self, id: &str) -> Result<providers::ProviderSummary, ClientError> {
        self.get(&format!("/providers/{id}")).await
    }
    pub async fn provider_accounts(&self, id: &str) -> Result<Vec<providers::ModelAccount>, ClientError> {
        self.get(&format!("/providers/{id}/accounts")).await
    }
    pub async fn provider_quota(&self, id: &str, query: &providers::QuotaQuery) -> Result<providers::ProviderQuotaReport, ClientError> {
        self.send(self.http.get(self.url(&format!("/providers/{id}/quota"))).query(query)).await
    }
    pub async fn provider_models(&self, id: &str) -> Result<Vec<providers::DiscoveredModel>, ClientError> {
        self.get(&format!("/providers/{id}/models")).await
    }
    pub async fn provider_key_groups(&self, id: &str) -> Result<Vec<providers::KeyGroup>, ClientError> {
        self.get(&format!("/providers/{id}/key-groups")).await
    }
    pub async fn provider_routing(&self, id: &str) -> Result<providers::RoutingView, ClientError> {
        self.get(&format!("/providers/{id}/routing")).await
    }
    pub async fn provider_logs(&self, id: &str) -> Result<Vec<providers::ProxyLogEntry>, ClientError> {
        self.get(&format!("/providers/{id}/logs")).await
    }
    pub async fn submit_provider_operation(
        &self,
        provider_id: &str,
        body: &providers::SubmitOperationIn,
    ) -> Result<providers::ProviderOperation, ClientError> {
        self.post(&format!("/providers/{provider_id}/operations"), body).await
    }
    pub async fn provider_operation(&self, op_id: Uuid) -> Result<providers::ProviderOperation, ClientError> {
        self.get(&format!("/providers/operations/{op_id}")).await
    }
    pub async fn ingest_provider_observations(
        &self,
        provider_id: &str,
        batch: &providers::ObservationBatch,
    ) -> Result<providers::ObservationAck, ClientError> {
        self.post(&format!("/providers/{provider_id}/observations"), batch).await
    }
    pub async fn claim_provider_operations(
        &self,
        body: &providers::ClaimOperationsIn,
    ) -> Result<Vec<providers::ProviderOperation>, ClientError> {
        self.post("/providers/operations/claim", body).await
    }
    pub async fn report_provider_operation_receipt(
        &self,
        op_id: Uuid,
        body: &providers::OperationReceiptIn,
    ) -> Result<providers::ProviderOperation, ClientError> {
        self.post(&format!("/providers/operations/{op_id}/receipt"), body).await
    }
    pub async fn ingest_activity(&self, batch: &providers::ActivityIngestBatch) -> Result<providers::ActivityIngestAck, ClientError> {
        self.post("/activity/ingest", batch).await
    }
    pub async fn activity_overview(&self, scope: &providers::ActivityScope) -> Result<providers::ActivityOverview, ClientError> {
        self.send(self.http.get(self.url("/activity/overview")).query(scope)).await
    }
    pub async fn activity_compare(&self, scope: &providers::ActivityScope) -> Result<providers::CompareReport, ClientError> {
        self.send(self.http.get(self.url("/activity/compare")).query(scope)).await
    }
    pub async fn activity_requests(&self, scope: &providers::ActivityScope) -> Result<providers::RequestPage, ClientError> {
        self.send(self.http.get(self.url("/activity/requests")).query(scope)).await
    }
    pub async fn activity_request_detail(&self, id: Uuid) -> Result<providers::RequestDetail, ClientError> {
        self.get(&format!("/activity/requests/{id}")).await
    }
    pub async fn activity_request_capture(&self, id: Uuid) -> Result<providers::CapturedBodyView, ClientError> {
        self.get(&format!("/activity/requests/{id}/capture")).await
    }
    pub async fn activity_traces(&self, scope: &providers::ActivityScope) -> Result<providers::TracePage, ClientError> {
        self.send(self.http.get(self.url("/activity/traces")).query(scope)).await
    }
    pub async fn activity_trace_detail(&self, session_id: &str) -> Result<providers::TraceDetail, ClientError> {
        self.get(&format!("/activity/traces/{session_id}")).await
    }
    pub async fn activity_drift(&self, scope: &providers::ActivityScope) -> Result<providers::DriftReport, ClientError> {
        self.send(self.http.get(self.url("/activity/drift")).query(scope)).await
    }
    pub async fn activity_totals(
        &self,
        granularity: providers::TotalsGranularity,
        scope: &providers::ActivityScope,
    ) -> Result<providers::TotalsReport, ClientError> {
        let g = serde_json::to_value(granularity).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "day".into());
        self.send(self.http.get(self.url("/activity/totals")).query(&[("granularity", g)]).query(scope)).await
    }
    pub async fn activity_thinking(&self, scope: &providers::ActivityScope) -> Result<providers::ThinkingReport, ClientError> {
        self.send(self.http.get(self.url("/activity/thinking")).query(scope)).await
    }
    pub async fn activity_evals(&self) -> Result<Vec<providers::EvalCase>, ClientError> {
        self.get("/activity/evals").await
    }
    pub async fn create_eval_case(&self, body: &providers::EvalCaseIn) -> Result<providers::EvalCase, ClientError> {
        self.post("/activity/evals", body).await
    }
    pub async fn update_eval_case(&self, id: Uuid, body: &providers::EvalCaseIn) -> Result<providers::EvalCase, ClientError> {
        self.put(&format!("/activity/evals/{id}"), body).await
    }
    pub async fn delete_eval_case(&self, id: Uuid) -> Result<serde_json::Value, ClientError> {
        self.delete(&format!("/activity/evals/{id}")).await
    }
    pub async fn replay_eval_case(&self, id: Uuid, body: &serde_json::Value) -> Result<providers::EvalReplayResult, ClientError> {
        self.post(&format!("/activity/evals/{id}/replay"), body).await
    }
    pub async fn export_eval_cases(&self, include_bodies: bool) -> Result<providers::EvalExportBundle, ClientError> {
        self.send(self.http.get(self.url("/activity/evals/export")).query(&[("include_bodies", include_bodies)])).await
    }
    pub async fn import_eval_cases(&self, bundle: &providers::EvalExportBundle) -> Result<Vec<providers::EvalCase>, ClientError> {
        self.post("/activity/evals/import", bundle).await
    }
    pub async fn capture_policy(&self) -> Result<providers::CapturePolicy, ClientError> {
        self.get("/activity/capture-policy").await
    }
    pub async fn set_capture_policy(&self, policy: &providers::CapturePolicy) -> Result<providers::CapturePolicy, ClientError> {
        self.put("/activity/capture-policy", policy).await
    }
    pub async fn clear_activity_data(&self, body: &providers::ClearDataIn) -> Result<providers::ClearDataOut, ClientError> {
        self.post("/activity/clear", body).await
    }
    pub async fn preview_diagnostic_bundle(
        &self,
        body: &providers::DiagnosticBundlePreviewIn,
    ) -> Result<providers::DiagnosticBundlePreview, ClientError> {
        self.post("/activity/exports/diagnostic-preview", body).await
    }
    pub async fn export_portable_archive(
        &self,
        body: &providers::PortableArchiveRequest,
    ) -> Result<providers::PortableArchiveExport, ClientError> {
        self.post("/activity/exports/portable-archive", body).await
    }
    pub async fn clickhouse_status(&self) -> Result<providers::ClickHouseStatus, ClientError> {
        self.get("/activity/sinks/clickhouse").await
    }
    pub async fn set_clickhouse_config(&self, config: &providers::ClickHouseConfig) -> Result<providers::ClickHouseStatus, ClientError> {
        self.put("/activity/sinks/clickhouse", config).await
    }
    pub async fn provider_settings(&self) -> Result<providers::ProviderPanelSettings, ClientError> {
        self.get("/providers/settings").await
    }
    pub async fn set_provider_settings(
        &self,
        settings: &providers::ProviderPanelSettings,
    ) -> Result<providers::ProviderPanelSettings, ClientError> {
        self.put("/providers/settings", settings).await
    }
}

/// The operator's own Paperclip board client.
///
/// H2 adds no privilege to the hub: the operator's key is read on their
/// machine and used here, and only the resulting native decision is linked
/// through [`HubClient::link_approval`]. `Debug` is manual so the key cannot
/// leak through logs.
#[derive(Clone)]
pub struct PaperclipBoard {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl std::fmt::Debug for PaperclipBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaperclipBoard").field("base", &self.base).field("key", &"<redacted>").finish()
    }
}

#[derive(Debug)]
pub enum PaperclipError {
    Config(String),
    Transport(String),
    Api { status: u16, message: String },
}

impl std::fmt::Display for PaperclipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaperclipError::Config(message) => write!(f, "paperclip config: {message}"),
            PaperclipError::Transport(message) => write!(f, "paperclip unreachable: {message}"),
            PaperclipError::Api { status, message } => write!(f, "paperclip {status}: {message}"),
        }
    }
}

impl std::error::Error for PaperclipError {}

impl PaperclipBoard {
    pub fn new(base: impl Into<String>, key: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self { http: reqwest::Client::new(), base, key: key.into() }
    }

    /// The key file and URL use the Paperclip CLI's own variables,
    /// `PAPERCLIP_API_KEY_FILE` and `PAPERCLIP_API_URL`. Without the first,
    /// the key is read from `$XDG_CONFIG_HOME/paperclip/api-key` (or
    /// `~/.config/paperclip/api-key`).
    pub fn from_env() -> Result<Self, PaperclipError> {
        let path = match std::env::var_os("PAPERCLIP_API_KEY_FILE").filter(|value| !value.is_empty()) {
            Some(file) => std::path::PathBuf::from(file),
            None => std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home| std::path::PathBuf::from(home).join(".config"))
                })
                .ok_or_else(|| PaperclipError::Config("set PAPERCLIP_API_KEY_FILE, XDG_CONFIG_HOME or HOME".into()))?
                .join("paperclip")
                .join("api-key"),
        };
        let key = std::fs::read_to_string(&path).map_err(|e| PaperclipError::Config(format!("reading {}: {e}", path.display())))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(PaperclipError::Config(format!("{} is empty", path.display())));
        }
        Ok(Self::new(std::env::var("PAPERCLIP_API_URL").unwrap_or_else(|_| "http://127.0.0.1:3117".into()), key))
    }

    /// Approve or reject one native approval as the board user. The body is
    /// `resolveApprovalSchema` (`{ decisionNote?: string }`) and the note is
    /// omitted when absent.
    pub async fn decide(&self, approval_id: &str, approve: bool, note: Option<&str>) -> Result<serde_json::Value, PaperclipError> {
        // The id is a path segment; anything else could change the request path.
        if approval_id.is_empty() || !approval_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(PaperclipError::Config(format!("approval id {approval_id:?} is not a safe path segment")));
        }
        let action = if approve { "approve" } else { "reject" };
        let mut body = serde_json::Map::new();
        if let Some(note) = note {
            body.insert("decisionNote".into(), serde_json::Value::String(note.to_string()));
        }
        let resp = self
            .http
            .post(format!("{}/api/approvals/{approval_id}/{action}", self.base))
            .bearer_auth(&self.key)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| PaperclipError::Transport(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| PaperclipError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(PaperclipError::Api { status: status.as_u16(), message: String::from_utf8_lossy(&bytes).into_owned() });
        }
        serde_json::from_slice(&bytes).map_err(|e| PaperclipError::Transport(format!("decode: {e}")))
    }
}
