//! Hub HTTP API. Handlers are thin: authenticate, attach the correlation id,
//! call the shared `hub-domain` command, map errors. Business rules live in
//! the domain so the UI, CLI and agent tools share one authorization path.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use contracts::*;
use domain::{Actor, DomainError, Hub};
use serde::Deserialize;
use uuid::Uuid;

pub mod bootstrap;

#[derive(Clone)]
pub struct AppState {
    pub hub: Hub,
    /// Present in fixture mode so the operator can act as the Paperclip board.
    pub fixture_paperclip: Option<Arc<integrations::fixture::FixturePaperclip>>,
}

pub struct ApiErr(DomainError, Option<String>);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            DomainError::Forbidden(_) => StatusCode::FORBIDDEN,
            DomainError::NotFound(_) => StatusCode::NOT_FOUND,
            DomainError::Conflict { .. } => StatusCode::CONFLICT,
            DomainError::Invalid { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            DomainError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            DomainError::Db(_) | DomainError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = match &self.0 {
            // Do not leak SQL details to callers.
            DomainError::Db(e) => {
                tracing::error!(error = %e, correlation_id = ?self.1, "database error");
                "database error".to_string()
            }
            e => e.to_string(),
        };
        (status, Json(ApiError { code: self.0.code().into(), message, correlation_id: self.1 })).into_response()
    }
}

type R<T> = Result<Json<T>, ApiErr>;

/// Authenticated caller extracted from `Authorization: Bearer`.
pub struct Caller(pub Actor);

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let correlation_id = parts
            .headers
            .get(CORRELATION_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 128)
            .map(String::from)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| ApiErr(DomainError::Forbidden("missing bearer token".into()), Some(correlation_id.clone())))?;
        let principal = domain::system::authenticate(&state.hub, token)
            .await
            .map_err(|e| ApiErr(e, Some(correlation_id.clone())))?
            .ok_or_else(|| ApiErr(DomainError::Forbidden("unknown token".into()), Some(correlation_id.clone())))?;
        tracing::Span::current().record("correlation_id", correlation_id.as_str());
        Ok(Caller(Actor { principal, correlation_id: Some(correlation_id) }))
    }
}

fn err(actor: &Actor) -> impl Fn(DomainError) -> ApiErr + '_ {
    move |e| ApiErr(e, actor.correlation_id.clone())
}

macro_rules! call {
    ($actor:expr, $e:expr) => {
        $e.await.map(Json).map_err(err(&$actor))
    };
}

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/status", get(status))
        .route("/system/pause", post(set_pause))
        .route("/today", get(today))
        .route("/search", get(search))
        .route("/history/{subject_type}/{id}", get(history))
        .route("/ingest/news", post(ingest_news))
        .route("/ingest/interactions", post(ingest_interaction))
        .route("/news", get(news_list))
        .route("/news/{id}", get(news_get))
        .route("/personas", get(personas).post(create_persona))
        .route("/personas/{id}/canon", get(canon_list).post(canon_propose))
        .route("/canon/{id}/decision", post(canon_decide))
        .route("/workflows", get(workflows))
        .route("/accounts", get(accounts))
        .route("/content", get(content_list).post(create_brief))
        .route("/content/{id}", get(content_detail))
        .route("/content/{id}/brief", post(update_brief))
        .route("/content/{id}/generations", post(generate))
        .route("/content/{id}/releases", post(freeze))
        .route("/generations/{id}/cancel", post(cancel_generation))
        .route("/generations/{id}/receipts", post(receipt))
        .route("/assets", get(assets))
        .route("/assets/{id}/select", post(select_asset))
        .route("/assets/{id}/content", get(asset_content))
        .route("/releases/{id}/request-review", post(request_review))
        .route("/releases/{id}/link-approval", post(link_approval))
        .route("/releases/{id}/schedule", post(schedule))
        .route("/intents", get(intents))
        .route("/intents/{id}/cancel", post(cancel_intent))
        .route("/intents/{id}/dispatch", post(dispatch))
        .route("/intents/{id}/resolve", post(resolve_intent))
        .route("/relationships", get(relationships))
        .route("/relationships/{id}", get(relationship))
        .route("/relationships/{id}/drafts", post(draft_reply))
        .route("/relationships/{id}/takeover", post(takeover))
        .route("/relationships/{id}/release", post(release_rel))
        .route("/relationships/{id}/handoff", post(handoff))
        .route("/relationships/{id}/follow-up", post(follow_up))
        .route("/drafts/{id}/decision", post(decide_draft))
        .route("/chat/{subject_type}/{id}", get(chat).post(send_chat))
        .route("/fixtures/paperclip/approvals", get(fixture_approvals))
        .route("/fixtures/paperclip/approvals/{id}/decide", post(fixture_decide));
    Router::new()
        .nest("/v1", v1)
        .route("/openapi.json", get(openapi))
        .route("/healthz", get(|| async { "ok" }))
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<Body>| {
            let cid = req.headers().get(CORRELATION_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            tracing::info_span!("http", method = %req.method(), path = %req.uri().path(), correlation_id = %cid)
        }))
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}

pub const OPENAPI: &str = include_str!("../openapi.json");

async fn openapi() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/json")], OPENAPI)
}

async fn status(State(s): State<AppState>, Caller(a): Caller) -> R<SystemStatus> {
    call!(a, domain::system::status(&s.hub, &a))
}
async fn set_pause(State(s): State<AppState>, Caller(a): Caller, Json(b): Json<PauseIn>) -> R<SystemStatus> {
    call!(a, domain::system::set_pause(&s.hub, &a, b))
}
async fn today(State(s): State<AppState>, Caller(a): Caller) -> R<Today> {
    call!(a, domain::content::today(&s.hub, &a))
}
#[derive(Deserialize)]
struct SearchQ {
    q: String,
}
async fn search(State(s): State<AppState>, Caller(a): Caller, Query(q): Query<SearchQ>) -> R<Vec<SearchHit>> {
    call!(a, domain::content::search(&s.hub, &a, &q.q))
}
async fn history(State(s): State<AppState>, Caller(a): Caller, Path((t, id)): Path<(String, String)>) -> R<Vec<HistoryEntry>> {
    a.require(domain::ANY_READER).map_err(err(&a))?;
    call!(a, domain::history(&s.hub.db, &t, &id))
}

fn idempotency(h: &HeaderMap) -> Option<String> {
    h.get(IDEMPOTENCY_HEADER).and_then(|v| v.to_str().ok()).map(String::from)
}

async fn ingest_news(State(s): State<AppState>, Caller(a): Caller, h: HeaderMap, Json(b): Json<NewsEventIn>) -> R<IngestReceipt> {
    call!(a, domain::news::ingest(&s.hub, &a, b, idempotency(&h)))
}
async fn ingest_interaction(State(s): State<AppState>, Caller(a): Caller, Json(b): Json<InteractionIn>) -> R<IngestReceipt> {
    call!(a, domain::engagement::ingest(&s.hub, &a, b))
}
async fn news_list(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<NewsItem>> {
    call!(a, domain::news::list(&s.hub, &a))
}
async fn news_get(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<NewsItem> {
    call!(a, domain::news::get(&s.hub, &a, id))
}
async fn personas(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<Persona>> {
    call!(a, domain::canon::list_personas(&s.hub, &a))
}
#[derive(Deserialize)]
struct NewPersona {
    name: String,
}
async fn create_persona(State(s): State<AppState>, Caller(a): Caller, Json(b): Json<NewPersona>) -> R<Persona> {
    call!(a, domain::canon::create_persona(&s.hub, &a, &b.name))
}
async fn canon_list(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<Vec<CanonRevision>> {
    call!(a, domain::canon::list(&s.hub, &a, id))
}
async fn canon_propose(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<CanonProposalIn>,
) -> R<CanonRevision> {
    call!(a, domain::canon::propose(&s.hub, &a, id, b))
}
async fn canon_decide(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<CanonDecisionIn>,
) -> R<CanonRevision> {
    call!(a, domain::canon::decide(&s.hub, &a, id, b))
}
async fn workflows(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<Workflow>> {
    call!(a, domain::production::workflows(&s.hub, &a))
}
async fn accounts(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<Account>> {
    call!(a, domain::system::accounts(&s.hub, &a))
}
async fn content_list(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<ContentItem>> {
    call!(a, domain::content::list(&s.hub, &a))
}
async fn create_brief(State(s): State<AppState>, Caller(a): Caller, Json(b): Json<CreateBriefIn>) -> R<ContentItem> {
    call!(a, domain::content::create_brief(&s.hub, &a, b))
}
async fn content_detail(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<ContentDetail> {
    call!(a, domain::content::detail(&s.hub, &a, id))
}
async fn update_brief(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<Brief>) -> R<ContentItem> {
    call!(a, domain::content::update_brief(&s.hub, &a, id, b))
}
async fn generate(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<GenerateIn>) -> R<GenerationRequest> {
    call!(a, domain::production::generate(&s.hub, &a, id, b))
}
async fn freeze(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<FreezeReleaseIn>) -> R<ReleaseRevision> {
    call!(a, domain::release::freeze(&s.hub, &a, id, b))
}
async fn cancel_generation(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<CancelOut> {
    call!(a, domain::production::cancel(&s.hub, &a, id))
}
async fn receipt(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<ReceiptIn>) -> R<GenerationRequest> {
    call!(a, domain::production::record_receipt(&s.hub, &a, id, b))
}
async fn assets(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<Asset>> {
    call!(a, domain::production::all_assets(&s.hub, &a))
}
async fn select_asset(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<SelectAssetIn>) -> R<Asset> {
    call!(a, domain::production::select_asset(&s.hub, &a, id, b))
}
async fn asset_content(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> Result<Response, ApiErr> {
    let (mime, bytes) = domain::production::asset_content(&s.hub, &a, id).await.map_err(err(&a))?;
    let mut resp = bytes.into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    // Content-addressed: safe to cache, never sniff.
    resp.headers_mut().insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    resp.headers_mut()
        .insert(axum::http::header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"));
    Ok(resp)
}
async fn request_review(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<ReleaseRevision> {
    call!(a, domain::release::request_review(&s.hub, &a, id))
}
async fn link_approval(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<LinkApprovalIn>,
) -> R<ReleaseRevision> {
    call!(a, domain::release::link_approval(&s.hub, &a, id, b))
}
async fn schedule(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<ScheduleIn>,
) -> R<Vec<PublicationIntent>> {
    call!(a, domain::release::schedule(&s.hub, &a, id, b))
}
async fn intents(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<PublicationIntent>> {
    call!(a, domain::release::all_intents(&s.hub, &a))
}
async fn cancel_intent(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<PublicationIntent> {
    call!(a, domain::release::cancel_intent(&s.hub, &a, id))
}
async fn dispatch(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<DispatchIn>) -> R<DispatchOut> {
    call!(a, domain::release::dispatch(&s.hub, &a, id, b))
}
async fn resolve_intent(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<ResolveIntentIn>,
) -> R<PublicationIntent> {
    call!(a, domain::release::resolve_intent(&s.hub, &a, id, b))
}
async fn relationships(State(s): State<AppState>, Caller(a): Caller) -> R<Vec<Relationship>> {
    call!(a, domain::engagement::list(&s.hub, &a))
}
async fn relationship(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<RelationshipDetail> {
    call!(a, domain::engagement::detail(&s.hub, &a, id))
}
async fn draft_reply(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<DraftReplyIn>) -> R<ReplyDraft> {
    call!(a, domain::engagement::draft_reply(&s.hub, &a, id, b))
}
async fn takeover(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<Relationship> {
    call!(a, domain::engagement::takeover(&s.hub, &a, id))
}
async fn release_rel(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<Relationship> {
    call!(a, domain::engagement::release_to_agent(&s.hub, &a, id))
}
async fn handoff(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<HandoffOut> {
    call!(a, domain::engagement::handoff(&s.hub, &a, id))
}
async fn follow_up(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<FollowUpIn>) -> R<Relationship> {
    call!(a, domain::engagement::set_follow_up(&s.hub, &a, id, b))
}
async fn decide_draft(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>, Json(b): Json<DraftDecisionIn>) -> R<ReplyDraft> {
    call!(a, domain::engagement::decide_draft(&s.hub, &a, id, b))
}
async fn chat(State(s): State<AppState>, Caller(a): Caller, Path((t, id)): Path<(String, Uuid)>) -> R<Vec<ChatMessage>> {
    call!(a, domain::content::chat(&s.hub, &a, &t, id))
}
async fn send_chat(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path((t, id)): Path<(String, Uuid)>,
    Json(b): Json<ChatMessageIn>,
) -> R<Vec<ChatMessage>> {
    call!(a, domain::content::send_chat(&s.hub, &a, &t, id, b))
}

// Fixture-only board actions. Absent (404) when Paperclip is live.
fn fixture(s: &AppState, a: &Actor) -> Result<Arc<integrations::fixture::FixturePaperclip>, ApiErr> {
    a.require(domain::OPERATOR).map_err(err(a))?;
    s.fixture_paperclip.clone().ok_or_else(|| ApiErr(DomainError::NotFound("fixture paperclip"), a.correlation_id.clone()))
}
async fn fixture_approvals(State(s): State<AppState>, Caller(a): Caller) -> Result<Json<serde_json::Value>, ApiErr> {
    let f = fixture(&s, &a)?;
    Ok(Json(serde_json::to_value(f.approvals()).unwrap_or_default()))
}
#[derive(Deserialize)]
struct Decide {
    approve: bool,
}
async fn fixture_decide(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<String>,
    Json(b): Json<Decide>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let f = fixture(&s, &a)?;
    let view = f
        .decide(&id, b.approve, &format!("fixture-board:{}", a.principal.name))
        .map_err(|e| ApiErr(DomainError::Invalid { code: "fixture", message: e.to_string() }, None))?;
    Ok(Json(serde_json::to_value(view).unwrap_or_default()))
}
