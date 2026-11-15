//! H2: the operator decides a native Paperclip approval with their own board
//! key, then the hub links it and re-checks the revision and payload hash.
//! Skips (passes trivially) when `HUB_TEST_DATABASE_URL` is unset.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use contracts::client::{ClientError, HubClient, PaperclipBoard};
use contracts::*;
use fleetix_hubd::bootstrap::{self, Settings};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Default)]
struct Mock {
    status: HashMap<String, String>,
    payload: HashMap<String, Value>,
    /// `(action, bearer, decisionNote)` per decision request.
    decisions: Vec<(String, String, Option<String>)>,
    next: u32,
}

type MockState = Arc<Mutex<Mock>>;

async fn create_approval(State(mock): State<MockState>, Json(body): Json<Value>) -> Json<Value> {
    let mut mock = mock.lock().await;
    mock.next += 1;
    let id = format!("approval-{}", mock.next);
    mock.status.insert(id.clone(), "pending".into());
    mock.payload.insert(id.clone(), body["payload"].clone());
    Json(json!({ "id": id, "status": "pending" }))
}

async fn read_approval(State(mock): State<MockState>, Path(id): Path<String>) -> Json<Value> {
    let mock = mock.lock().await;
    let status = mock.status.get(&id).cloned().unwrap_or_else(|| "missing".into());
    let payload = mock.payload.get(&id).cloned().unwrap_or(Value::Null);
    let mut body = json!({ "id": id, "status": status, "payload": payload });
    if status != "pending" {
        body["decidedByUserId"] = json!("board-user-1");
        body["decidedAt"] = json!(Utc::now().to_rfc3339());
    }
    Json(body)
}

async fn decide(mock: MockState, id: String, headers: HeaderMap, body: Value, approve: bool) -> Json<Value> {
    let mut mock = mock.lock().await;
    let action = if approve { "approve" } else { "reject" };
    let bearer = headers.get(axum::http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let note = body.get("decisionNote").and_then(Value::as_str).map(str::to_string);
    mock.decisions.push((action.to_string(), bearer, note));
    mock.status.insert(id.clone(), if approve { "approved" } else { "rejected" }.into());
    Json(json!({ "id": id, "status": mock.status.get(&id) }))
}

async fn approve(State(mock): State<MockState>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    decide(mock, id, headers, body, true).await
}

async fn reject(State(mock): State<MockState>, Path(id): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    decide(mock, id, headers, body, false).await
}

struct H {
    op: HubClient,
    hub: domain::Hub,
    server: tokio::task::JoinHandle<()>,
    mock: MockState,
    mock_url: String,
}

async fn fresh_db() -> Option<String> {
    let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return None;
    };
    let admin = domain::sqlx::PgPool::connect(&base).await.expect("connect admin db");
    let name = format!("t_{}", Uuid::new_v4().simple());
    domain::sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let (head, query) = base.split_once('?').map(|(h, q)| (h.to_string(), format!("?{q}"))).unwrap_or((base.clone(), String::new()));
    let head = head.rsplit_once('/').map(|(h, _)| h.to_string()).unwrap();
    Some(format!("{head}/{name}{query}"))
}

async fn harness() -> Option<H> {
    let db_url = fresh_db().await?;
    let assets = std::env::temp_dir().join(format!("ihub-approval-{}", Uuid::new_v4()));

    // A Paperclip stand-in: the hub uses its live port, the operator uses
    // their own board key. Nobody sends the operator key to the hub.
    let mock: MockState = Arc::new(Mutex::new(Mock::default()));
    let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_url = format!("http://{}", mock_listener.local_addr().unwrap());
    let mock_routes = Router::new()
        .route("/api/companies/{company}/approvals", post(create_approval))
        .route("/api/approvals/{id}", get(read_approval))
        .route("/api/approvals/{id}/approve", post(approve))
        .route("/api/approvals/{id}/reject", post(reject))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(mock_listener, mock_routes).await.unwrap() });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let mut s = Settings::fixture(&db_url, &addr);
    s.asset_dir = assets;
    s.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
    s.seed_file = Some(concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry/hub.fixture.json").into());
    let (mut state, _fixtures) = bootstrap::build(&s).await.unwrap();
    // The hub keeps its own token; the operator's key never reaches it.
    state.hub.paperclip = Arc::new(integrations::live::PaperclipClient::new(
        reqwest::Client::new(),
        integrations::live::PaperclipConfig::new(mock_url.clone(), "hub-token"),
    ));
    for (name, role, token) in [("operator", Role::Operator, "op"), ("agent", Role::Agent, "ag")] {
        domain::system::issue_token(&state.hub.db, name, role, Some(token.into())).await.unwrap();
    }
    let hub = state.hub.clone();
    let url = state.hub.config.public_url.clone();
    let server = tokio::spawn(async move { axum::serve(listener, fleetix_hubd::router(state)).await.unwrap() });
    Some(H { op: HubClient::new(&url, "op"), hub, server, mock, mock_url })
}

fn code(error: ClientError) -> String {
    match error {
        ClientError::Api { error, .. } => error.code,
        other => panic!("expected API error, got {other}"),
    }
}

async fn freeze(h: &H, content: Uuid, text: &str) -> ReleaseRevision {
    h.op.freeze_release(
        content,
        &FreezeReleaseIn {
            account_ids: vec!["fixture-mastodon".into()],
            text: text.into(),
            platform_settings: json!({}),
            reply_to: None,
            timing: TimingScope { not_before: Utc::now(), not_after: Utc::now() + chrono::Duration::hours(2) },
        },
    )
    .await
    .unwrap()
}

async fn content(h: &H) -> ContentItem {
    let persona = h.op.personas().await.unwrap().remove(0);
    h.op.create_brief(&CreateBriefIn {
        persona_id: persona.id,
        news_item_id: None,
        title: "Sorrel release".into(),
        brief: Brief {
            angle: "a".into(),
            audience: "b".into(),
            copy_direction: "c".into(),
            visual_direction: "d".into(),
            producer: Producer::Direct,
        },
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn operator_decision_links_exactly_the_reviewed_revision() {
    let Some(h) = harness().await else { return };
    let item = content(&h).await;
    let release = freeze(&h, item.id, "First frozen text").await;
    h.op.request_review(release.id).await.unwrap();
    worker::tick(&h.hub).await.unwrap();
    let release = h.op.release(release.id).await.unwrap();
    let approval_id = release.paperclip_approval_id.clone().expect("the live port created the approval");

    let board = PaperclipBoard::new(h.mock_url.clone(), "operator-board-key");
    board.decide(&approval_id, true, Some("ship it")).await.expect("the operator approves with their own key");

    {
        let mock = h.mock.lock().await;
        assert_eq!(mock.decisions, vec![("approve".to_string(), "Bearer operator-board-key".to_string(), Some("ship it".to_string()))]);
    }

    let linked = h.op.link_approval(release.id, &LinkApprovalIn { approval_id: approval_id.clone() }).await.unwrap();
    assert_eq!(linked.state, ReleaseState::Approved);
    assert_eq!(linked.approved_by.as_deref(), Some("paperclip:board-user-1"));
    assert_eq!(linked.paperclip_approval_id.as_deref(), Some(approval_id.as_str()));
    h.server.abort();
}

#[tokio::test]
async fn a_payload_change_after_the_decision_makes_the_link_fail() {
    let Some(h) = harness().await else { return };
    let item = content(&h).await;
    let release = freeze(&h, item.id, "First frozen text").await;
    h.op.request_review(release.id).await.unwrap();
    worker::tick(&h.hub).await.unwrap();
    let approval_id = h.op.release(release.id).await.unwrap().paperclip_approval_id.expect("approval created");

    PaperclipBoard::new(h.mock_url.clone(), "operator-board-key").decide(&approval_id, true, None).await.expect("the operator approves");

    // Editing means freezing a new revision; the old approval must not carry.
    let newer = freeze(&h, item.id, "Edited frozen text").await;
    let error = h.op.link_approval(newer.id, &LinkApprovalIn { approval_id }).await.unwrap_err();
    assert_eq!(code(error), "approval_payload_mismatch");
    h.server.abort();
}

#[tokio::test]
async fn a_rejected_approval_links_as_rejected() {
    let Some(h) = harness().await else { return };
    let item = content(&h).await;
    let release = freeze(&h, item.id, "First frozen text").await;
    h.op.request_review(release.id).await.unwrap();
    worker::tick(&h.hub).await.unwrap();
    let approval_id = h.op.release(release.id).await.unwrap().paperclip_approval_id.expect("approval created");

    PaperclipBoard::new(h.mock_url.clone(), "operator-board-key").decide(&approval_id, false, Some("not yet")).await.unwrap();
    let linked = h.op.link_approval(release.id, &LinkApprovalIn { approval_id }).await.unwrap();
    assert_eq!(linked.state, ReleaseState::Rejected);
    h.server.abort();
}

#[tokio::test]
async fn linking_after_the_worker_applied_the_decision_returns_it() {
    let Some(h) = harness().await else { return };
    let item = content(&h).await;
    let release = freeze(&h, item.id, "First frozen text").await;
    h.op.request_review(release.id).await.unwrap();
    worker::tick(&h.hub).await.unwrap();
    let approval_id = h.op.release(release.id).await.unwrap().paperclip_approval_id.expect("approval created");

    PaperclipBoard::new(h.mock_url.clone(), "operator-board-key").decide(&approval_id, true, None).await.unwrap();
    // The worker's reconciliation applies the decision before the client links it.
    worker::tick(&h.hub).await.unwrap();
    assert_eq!(h.op.release(release.id).await.unwrap().state, ReleaseState::Approved);

    let linked = h.op.link_approval(release.id, &LinkApprovalIn { approval_id }).await.unwrap();
    assert_eq!(linked.state, ReleaseState::Approved);
    h.server.abort();
}
