//! Hub HTTP API. Handlers are thin: authenticate, attach the correlation id,
//! call the shared `hub-domain` command, map errors. Business rules live in
//! the domain so the UI, CLI and agent tools share one authorization path.

use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use contracts::*;
use domain::{Actor, DomainError, Hub};
use plugin::{Pack, ServerContext, ServerHost, valid_pack_id, validate_packs};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

pub mod bootstrap;
pub mod mcp;
pub mod plugin;

#[derive(Clone)]
pub struct AppState {
    pub hub: Hub,
    /// Present in fixture mode so the operator can act as the Paperclip board.
    pub fixture_paperclip: Option<Arc<integrations::fixture::FixturePaperclip>>,
    /// Fleet snapshot path; `None` means the hub serves no fleet section.
    pub fleet_status_file: Option<std::path::PathBuf>,
}

/// An API failure in the wire error shape. Domain outcomes keep their stable
/// codes; `coded` covers failures outside the domain, such as an unreadable
/// fleet snapshot.
pub struct ApiErr {
    status: StatusCode,
    code: &'static str,
    message: String,
    correlation_id: Option<String>,
}

impl ApiErr {
    /// Map a domain outcome onto the wire error shape. Pack routes use this so
    /// their errors look like core ones.
    pub fn domain(error: DomainError, correlation_id: Option<String>) -> Self {
        let status = match &error {
            DomainError::Forbidden(_) => StatusCode::FORBIDDEN,
            DomainError::NotFound(_) => StatusCode::NOT_FOUND,
            DomainError::Conflict { .. } => StatusCode::CONFLICT,
            DomainError::Invalid { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            DomainError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            DomainError::Db(_) | DomainError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = match &error {
            // Do not leak SQL details to callers.
            DomainError::Db(e) => {
                tracing::error!(error = %e, correlation_id = ?correlation_id, "database error");
                "database error".to_string()
            }
            e => e.to_string(),
        };
        Self { status, code: error.code(), message, correlation_id }
    }

    fn coded(status: StatusCode, code: &'static str, message: impl Into<String>, correlation_id: Option<String>) -> Self {
        Self { status, code, message: message.into(), correlation_id }
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        let error = ApiError { code: self.code.into(), message: self.message, correlation_id: self.correlation_id };
        (self.status, Json(error)).into_response()
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
            .ok_or_else(|| ApiErr::domain(DomainError::Forbidden("missing bearer token".into()), Some(correlation_id.clone())))?;
        let principal = domain::system::authenticate(&state.hub, token)
            .await
            .map_err(|e| ApiErr::domain(e, Some(correlation_id.clone())))?
            .ok_or_else(|| ApiErr::domain(DomainError::Forbidden("unknown token".into()), Some(correlation_id.clone())))?;
        tracing::Span::current().record("correlation_id", correlation_id.as_str());
        Ok(Caller(Actor { principal, correlation_id: Some(correlation_id) }))
    }
}

fn err(actor: &Actor) -> impl Fn(DomainError) -> ApiErr + '_ {
    move |e| ApiErr::domain(e, actor.correlation_id.clone())
}

macro_rules! call {
    ($actor:expr, $e:expr) => {
        $e.await.map(Json).map_err(err(&$actor))
    };
}

pub fn router(state: AppState) -> Router {
    router_with(state, Vec::new())
}

/// Build the HTTP router, nesting each pack at `/v1/p/<id>`. Pack routers are
/// `Router<AppState>`; the state is attached once, after registration.
pub fn router_with(state: AppState, pack_routes: Vec<(String, Router<AppState>)>) -> Router {
    let mut v1 = Router::new()
        .route("/status", get(status))
        .route("/fleet/status", get(fleet_status))
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
        // Artifact bytes (renders, videos) are far past the JSON body limit.
        .route("/generations/{id}/artifacts", post(upload_artifact).layer(axum::extract::DefaultBodyLimit::max(MAX_ARTIFACT_BYTES)))
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
        .route("/relationships/{id}/draft-reply", post(draft_assistant_reply))
        .route("/relationships/{id}/takeover", post(takeover))
        .route("/relationships/{id}/release", post(release_rel))
        .route("/relationships/{id}/handoff", post(handoff))
        .route("/relationships/{id}/follow-up", post(follow_up))
        .route("/drafts/{id}/decision", post(decide_draft))
        .route("/chat/{subject_type}/{id}", get(chat).post(send_chat))
        .route("/fixtures/paperclip/approvals", get(fixture_approvals))
        .route("/fixtures/paperclip/approvals/{id}/decide", post(fixture_decide));
    for (id, routes) in pack_routes {
        v1 = v1.nest(&format!("/p/{id}"), routes);
    }
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

/// Library entry point used by `fleetix-hubd serve` and `canix-hub serve`.
///
/// Builds the state exactly as the upstream binary does, installs the packs
/// enabled by `packs.json`, mounts their routes and serves until Ctrl-C.
/// Startup fails when `packs.json` enables an id this binary lacks.
pub async fn run(settings: bootstrap::Settings, packs: Vec<Box<dyn Pack>>) -> anyhow::Result<()> {
    use anyhow::Context as _;

    validate_packs(&packs)?;
    let (mut state, _fixtures) = bootstrap::build(&settings).await?;
    let configs = read_packs_config(settings.packs_config.as_deref())?;
    for id in configs.keys() {
        anyhow::ensure!(packs.iter().any(|p| p.id() == id), "packs.json enables unknown pack {id:?}; this binary has no such pack");
    }
    let base = ServerContext {
        db: state.hub.db.clone(),
        // Same bounds as the built-in adapters: a pack call can never hang
        // the worker while it holds an outbox row.
        http: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(60))
            .build()?,
        public_url: settings.public_url.clone(),
        state_dir: settings.asset_dir.clone(),
        config: Value::Null,
        credentials_dir: std::env::var_os("CREDENTIALS_DIRECTORY")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| settings.asset_dir.join("credentials")),
    };
    let mut channels = (*state.hub.channels).clone();
    let mut pipelines = (*state.hub.pipelines).clone();
    let mut topics: BTreeMap<String, Arc<dyn domain::outbox::OutboxHandler>> = BTreeMap::new();
    let mut routes = Vec::new();
    let mut integrations = state.hub.config.integrations.clone();
    for pack in &packs {
        let Some(config) = configs.get(pack.id()) else { continue };
        let ctx = ServerContext { config: config.clone(), ..base.clone() };
        let mut host = ServerHost::new(pack.id(), &ctx);
        pack.install(&mut host)?;
        let regs = host.finish();
        merge_pack_channels(regs.pack_id, &mut channels, regs.channels, settings.publisher == AdapterMode::Fixture, &state.hub.publisher)?;
        for (id, pipeline) in regs.pipelines {
            anyhow::ensure!(!pipelines.contains_key(&id), "pack {}: pipeline {id} is registered twice", regs.pack_id);
            pipelines.insert(id, pipeline);
        }
        for (topic, handler) in regs.topics {
            anyhow::ensure!(!topics.contains_key(&topic), "pack {}: topic {topic} is registered twice", regs.pack_id);
            topics.insert(topic, handler);
        }
        if let Some(router) = regs.routes {
            routes.push((regs.pack_id.to_string(), router));
        }
        integrations.extend(regs.integrations);
    }
    // A pack channel is live even when `HUB_PUBLISHER` is a fixture adapter, so
    // the approval guard runs again over the complete channel registry.
    bootstrap::require_live_approvals(settings.paperclip, settings.publisher, &state.hub.publisher, &channels)
        .context("publishing channels")?;
    let mut hub = state.hub.clone();
    hub.config = Arc::new(domain::HubConfig { integrations, ..(*state.hub.config).clone() });
    hub.channels = Arc::new(channels);
    hub.pipelines = Arc::new(pipelines);
    hub.topics = Arc::new(topics);
    state.hub = hub;

    if !settings.no_worker {
        tokio::spawn(worker::run(state.hub.clone(), Duration::from_secs(2)));
    }
    let listener = tokio::net::TcpListener::bind(&settings.bind).await.with_context(|| format!("binding {}", settings.bind))?;
    tracing::info!(bind = %settings.bind, "listening");
    axum::serve(listener, router_with(state, routes))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Merge one pack's channels into the registry.
///
/// A pack channel posts for real, so it may replace a fixture-only alias (a
/// seeded account transport or the built-in `fixture` name) instead of
/// colliding with it. A collision with a live built-in or an earlier pack still
/// fails startup.
fn merge_pack_channels(
    pack: &str,
    channels: &mut BTreeMap<String, Arc<dyn domain::ports::Publisher>>,
    incoming: BTreeMap<String, Arc<dyn domain::ports::Publisher>>,
    fixture_only: bool,
    builtin: &Arc<dyn domain::ports::Publisher>,
) -> anyhow::Result<()> {
    for (name, publisher) in incoming {
        let replaces_fixture_alias = fixture_only && channels.get(&name).is_some_and(|existing| Arc::ptr_eq(existing, builtin));
        anyhow::ensure!(
            replaces_fixture_alias || !channels.contains_key(&name),
            "pack {pack}: channel {name} collides with a built-in channel or an earlier pack"
        );
        channels.insert(name, publisher);
    }
    Ok(())
}

/// `packs.json`: a JSON object mapping pack id to that pack's config object.
fn read_packs_config(path: Option<&FsPath>) -> anyhow::Result<BTreeMap<String, Value>> {
    use anyhow::Context as _;

    let Some(path) = path else { return Ok(BTreeMap::new()) };
    let body = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let configs: BTreeMap<String, Value> = serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    for id in configs.keys() {
        anyhow::ensure!(valid_pack_id(id), "pack id {id:?} must match [a-z0-9-]+");
    }
    Ok(configs)
}

pub const OPENAPI: &str = include_str!("../openapi.json");

async fn openapi() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/json")], OPENAPI)
}

async fn status(State(s): State<AppState>, Caller(a): Caller) -> R<SystemStatus> {
    call!(a, domain::system::status(&s.hub, &a))
}

/// Why the fleet snapshot could not be served. The codes are stable: the
/// desktop and CLI branch on them.
#[derive(Debug, PartialEq, Eq)]
pub enum FleetStatusError {
    /// `HUB_FLEET_STATUS_FILE` is not set.
    Unconfigured,
    /// The file is missing or unreadable.
    Unavailable(String),
    /// Not a snapshot, or a schema version this hub does not know.
    Invalid(String),
}

/// Read the snapshot another tool rewrites atomically every few minutes.
///
/// Re-read per request: the file is small, and a cache would only add
/// staleness. `tokio::fs` keeps the read off the runtime threads.
pub async fn read_fleet_status(path: Option<&FsPath>) -> Result<contracts::fleet::FleetStatus, FleetStatusError> {
    let Some(path) = path else { return Err(FleetStatusError::Unconfigured) };
    let body = tokio::fs::read_to_string(path).await.map_err(|e| FleetStatusError::Unavailable(format!("{}: {e}", path.display())))?;
    let status: contracts::fleet::FleetStatus = serde_json::from_str(&body).map_err(|e| FleetStatusError::Invalid(e.to_string()))?;
    if status.schema_version != contracts::fleet::SCHEMA_VERSION {
        return Err(FleetStatusError::Invalid(format!(
            "schemaVersion {} is not {}",
            status.schema_version,
            contracts::fleet::SCHEMA_VERSION
        )));
    }
    Ok(status)
}

fn fleet_error(error: FleetStatusError, correlation_id: Option<String>) -> ApiErr {
    match error {
        FleetStatusError::Unconfigured => {
            ApiErr::coded(StatusCode::NOT_FOUND, "fleet_status_unconfigured", "HUB_FLEET_STATUS_FILE is not set", correlation_id)
        }
        FleetStatusError::Unavailable(message) => {
            ApiErr::coded(StatusCode::SERVICE_UNAVAILABLE, "fleet_status_unavailable", message, correlation_id)
        }
        FleetStatusError::Invalid(message) => ApiErr::coded(StatusCode::BAD_GATEWAY, "fleet_status_invalid", message, correlation_id),
    }
}

/// The snapshot is infrastructure state, not content: any authenticated
/// principal may read it, and the hub never collects fleet data itself.
async fn fleet_status(State(s): State<AppState>, Caller(a): Caller) -> R<contracts::fleet::FleetStatus> {
    read_fleet_status(s.fleet_status_file.as_deref()).await.map(Json).map_err(|e| fleet_error(e, a.correlation_id.clone()))
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

/// Largest artifact a pipeline may upload. The bytes are buffered while the
/// hash is verified and the object is finalized, so this is a memory bound too.
const MAX_ARTIFACT_BYTES: usize = 256 * 1024 * 1024;

fn header(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// `POST /v1/generations/{id}/artifacts`: raw output bytes with
/// `x-artifact-source`, `content-type` and `x-sha256`.
async fn upload_artifact(
    State(s): State<AppState>,
    Caller(a): Caller,
    Path(id): Path<Uuid>,
    h: HeaderMap,
    body: axum::body::Bytes,
) -> R<GenerationRequest> {
    let source = header(&h, "x-artifact-source").unwrap_or_default();
    let sha256 = header(&h, "x-sha256").unwrap_or_default();
    let mime = header(&h, axum::http::header::CONTENT_TYPE.as_str()).unwrap_or_else(|| "application/octet-stream".into());
    call!(a, domain::production::upload_artifact(&s.hub, &a, id, &source, &mime, &sha256, &body))
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
async fn draft_assistant_reply(State(s): State<AppState>, Caller(a): Caller, Path(id): Path<Uuid>) -> R<ReplyDraft> {
    call!(a, domain::engagement::draft_assistant_reply(&s.hub, &a, id))
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
    s.fixture_paperclip.clone().ok_or_else(|| ApiErr::domain(DomainError::NotFound("fixture paperclip"), a.correlation_id.clone()))
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
        .map_err(|e| ApiErr::domain(DomainError::Invalid { code: "fixture", message: e.to_string() }, None))?;
    Ok(Json(serde_json::to_value(view).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("fleet-status-{}-{name}.json", Uuid::new_v4()));
        std::fs::write(&path, body).unwrap();
        path
    }

    const SNAPSHOT: &str = r#"{
      "schemaVersion": 1,
      "generatedAt": "2026-11-10T03:10:00Z",
      "targetRevision": "new",
      "hosts": [
        {"name": "atlas", "health": "healthy", "revision": "new", "checkedAt": "2026-11-10T03:09:00Z"},
        {"name": "nomad", "health": "healthy", "revision": "old", "checkedAt": "2026-11-10T03:09:00Z"}
      ],
      "monitoring": {"monitored": 4, "alerts": []}
    }"#;

    #[test]
    fn a_pack_channel_replaces_a_fixture_alias_but_not_a_live_one() {
        let builtin: Arc<dyn domain::ports::Publisher> = Arc::new(integrations::fixture::FixturePublisher::default());
        let pack: Arc<dyn domain::ports::Publisher> = Arc::new(integrations::fixture::FixturePublisher::default());
        let mut channels = BTreeMap::from([("plinth".to_string(), builtin.clone()), ("fixture".to_string(), builtin.clone())]);

        // Fixture mode: the pack wins over a seeded transport alias and over
        // the built-in `fixture` name.
        for name in ["plinth", "fixture"] {
            let incoming = BTreeMap::from([(name.to_string(), pack.clone())]);
            merge_pack_channels("plinth", &mut channels, incoming, true, &builtin).unwrap();
            assert!(Arc::ptr_eq(channels.get(name).unwrap(), &pack), "{name} belongs to the pack");
        }

        // A live collision is still a startup failure: an earlier pack's
        // channel, and the built-in adapter outside fixture mode.
        let mut taken = BTreeMap::from([("plinth".to_string(), pack.clone())]);
        let err = merge_pack_channels("studio", &mut taken, BTreeMap::from([("plinth".to_string(), pack.clone())]), true, &builtin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("studio") && err.contains("plinth"), "the pack and channel are named: {err}");

        let mut live = BTreeMap::from([("postiz".to_string(), builtin.clone())]);
        let err = merge_pack_channels("plinth", &mut live, BTreeMap::from([("postiz".to_string(), pack.clone())]), false, &builtin)
            .unwrap_err()
            .to_string();
        assert!(err.contains("plinth") && err.contains("postiz"), "the pack and channel are named: {err}");
    }

    #[tokio::test]
    async fn fleet_status_reads_the_snapshot_and_maps_its_failures() {
        let ok = temp_file("ok", SNAPSHOT);
        let status = read_fleet_status(Some(&ok)).await.unwrap();
        assert_eq!(status.hosts.len(), 2);
        assert_eq!(status.attention(), 1, "nomad is outdated");
        std::fs::remove_file(&ok).unwrap();

        assert!(matches!(read_fleet_status(None).await, Err(FleetStatusError::Unconfigured)));
        let missing = std::env::temp_dir().join(format!("missing-{}.json", Uuid::new_v4()));
        assert!(matches!(read_fleet_status(Some(&missing)).await, Err(FleetStatusError::Unavailable(_))));

        let broken = temp_file("broken", "{ not json");
        assert!(matches!(read_fleet_status(Some(&broken)).await, Err(FleetStatusError::Invalid(_))));
        std::fs::remove_file(&broken).unwrap();

        let stale_schema = temp_file("schema", &SNAPSHOT.replace("\"schemaVersion\": 1", "\"schemaVersion\": 2"));
        assert!(matches!(read_fleet_status(Some(&stale_schema)).await, Err(FleetStatusError::Invalid(_))));
        std::fs::remove_file(&stale_schema).unwrap();

        for (error, want_status, want_code) in [
            (FleetStatusError::Unconfigured, StatusCode::NOT_FOUND, "fleet_status_unconfigured"),
            (FleetStatusError::Unavailable("gone".into()), StatusCode::SERVICE_UNAVAILABLE, "fleet_status_unavailable"),
            (FleetStatusError::Invalid("bad".into()), StatusCode::BAD_GATEWAY, "fleet_status_invalid"),
        ] {
            let api = fleet_error(error, None);
            assert_eq!(api.status, want_status);
            assert_eq!(api.code, want_code);
        }
    }
}
