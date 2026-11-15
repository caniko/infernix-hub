//! End-to-end behaviour over HTTP with fixture adapters and a real
//! PostgreSQL. Skips (passes trivially, with a note) when
//! `HUB_TEST_DATABASE_URL` is unset — see `scripts/dev-db.sh`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use contracts::client::{ClientError, HubClient};
use contracts::*;
use fleetix_hubd::bootstrap::{self, Fixtures, Settings};
use integrations::fixture::{FixturePublisher, PublishMode};
use serde_json::json;
use uuid::Uuid;

struct H {
    op: HubClient,
    agent: HubClient,
    procedure: HubClient,
    ingest: HubClient,
    fx: Fixtures,
    hub: domain::Hub,
    db_url: String,
    assets: std::path::PathBuf,
    url: String,
    server: tokio::task::JoinHandle<()>,
}

async fn fresh_db() -> Option<String> {
    let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return None;
    };
    let admin = domain::sqlx::PgPool::connect(&base).await.expect("connect admin db");
    let name = format!("t_{}", Uuid::new_v4().simple());
    domain::sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    // `.../hub?host=..` → `.../t_xxx?host=..`
    let (head, query) = base.split_once('?').map(|(h, q)| (h.to_string(), format!("?{q}"))).unwrap_or((base.clone(), String::new()));
    let head = head.rsplit_once('/').map(|(h, _)| h.to_string()).unwrap();
    Some(format!("{head}/{name}{query}"))
}

async fn start(db_url: &str, assets: &std::path::Path) -> H {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let mut s = Settings::fixture(db_url, &addr);
    s.asset_dir = assets.to_path_buf();
    s.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
    s.seed_file = Some(concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry/hub.fixture.json").into());
    let (state, fx) = bootstrap::build(&s).await.unwrap();
    for (name, role, token) in
        [("operator", Role::Operator, "op"), ("agent", Role::Agent, "ag"), ("windmill", Role::Procedure, "pr"), ("n8n", Role::Ingest, "in")]
    {
        domain::system::issue_token(&state.hub.db, name, role, Some(token.into())).await.unwrap();
    }
    let w = fx.windmill.clone().unwrap();
    w.connect(&s.public_url, "pr");
    w.manual_timers.store(true, std::sync::atomic::Ordering::SeqCst);
    let hub = state.hub.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, fleetix_hubd::router(state)).await.unwrap();
    });
    let url = s.public_url.clone();
    H {
        op: HubClient::new(&url, "op"),
        agent: HubClient::new(&url, "ag"),
        procedure: HubClient::new(&url, "pr"),
        ingest: HubClient::new(&url, "in"),
        fx,
        hub,
        db_url: db_url.into(),
        assets: assets.into(),
        url,
        server,
    }
}

async fn harness() -> Option<H> {
    let db = fresh_db().await?;
    let assets = std::env::temp_dir().join(format!("ihub-test-{}", Uuid::new_v4()));
    Some(start(&db, &assets).await)
}

fn code(e: ClientError) -> String {
    match e {
        ClientError::Api { error, .. } => error.code,
        other => panic!("expected API error, got {other}"),
    }
}

impl H {
    async fn tick(&self) {
        worker::tick(&self.hub).await.unwrap();
    }

    async fn persona(&self) -> Persona {
        self.op.personas().await.unwrap().remove(0)
    }

    async fn news(&self, id: &str, url: &str) -> IngestReceipt {
        let ev = NewsEventIn {
            source: "n8n:test".into(),
            external_id: id.into(),
            url: url.into(),
            title: format!("Story {id}"),
            summary: "Something happened".into(),
            published_at: None,
            evidence: vec![Evidence { url: url.into(), quote: "a quote".into() }],
            raw: json!({}),
        };
        self.ingest.post("/ingest/news", &ev).await.unwrap()
    }

    async fn brief(&self, news: Option<Uuid>) -> ContentItem {
        self.brief_with(news, Producer::Delegated).await
    }

    async fn brief_with(&self, news: Option<Uuid>, producer: Producer) -> ContentItem {
        let p = self.persona().await;
        self.op
            .create_brief(&CreateBriefIn {
                persona_id: p.id,
                news_item_id: news,
                title: "Kiln story".into(),
                brief: Brief {
                    angle: "craft".into(),
                    audience: "locals".into(),
                    copy_direction: "warm".into(),
                    visual_direction: "ember glow".into(),
                    producer,
                },
            })
            .await
            .unwrap()
    }

    async fn workflow(&self, name: &str) -> Workflow {
        self.op.workflows().await.unwrap().into_iter().find(|w| w.name == name).unwrap()
    }

    /// Generate, let the fixture procedure submit, run the engine, import.
    async fn produce(&self, content: Uuid, variants: i32) -> GenerationRequest {
        let wf = self.workflow("poster-txt2img").await;
        let g = self
            .op
            .generate(content, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "a kiln at dusk" }), variants })
            .await
            .unwrap();
        self.tick().await; // outbox → fixture Windmill procedure
        self.wait_queued(variants as usize).await;
        let comfy = self.fx.comfy.as_ref().unwrap();
        for _ in 0..(variants as u64 * (comfy.steps + 1)) {
            comfy.tick();
        }
        self.tick().await; // reconcile history + import
        self.generation(content, g.id).await
    }

    async fn wait_queued(&self, n: usize) {
        let comfy = self.fx.comfy.as_ref().unwrap();
        for _ in 0..100 {
            if comfy.queued().len() + comfy.running().iter().count() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("procedure did not submit {n} prompts");
    }

    async fn generation(&self, content: Uuid, id: Uuid) -> GenerationRequest {
        self.op.content_detail(content).await.unwrap().generations.into_iter().find(|g| g.id == id).unwrap()
    }

    /// Select all ready assets with alt text.
    async fn select_all(&self, content: Uuid) {
        for a in self.op.content_detail(content).await.unwrap().assets {
            self.op.select_asset(a.id, &SelectAssetIn { selected: true, alt_text: Some("A kiln glowing at dusk".into()) }).await.unwrap();
        }
    }

    async fn freeze(&self, content: Uuid, accounts: &[&str], text: &str) -> ReleaseRevision {
        let now = Utc::now();
        self.op
            .freeze_release(
                content,
                &FreezeReleaseIn {
                    account_ids: accounts.iter().map(|s| s.to_string()).collect(),
                    text: text.into(),
                    platform_settings: json!({}),
                    reply_to: None,
                    timing: TimingScope { not_before: now - chrono::Duration::hours(1), not_after: now + chrono::Duration::hours(6) },
                },
            )
            .await
            .unwrap()
    }

    async fn approve(&self, release: &ReleaseRevision) -> ReleaseRevision {
        self.op.request_review(release.id).await.unwrap();
        self.tick().await; // create native approval
        let rel = self.release(release.content_item_id, release.id).await;
        let approval = rel.paperclip_approval_id.clone().expect("approval created");
        let _: serde_json::Value =
            self.op.post(&format!("/fixtures/paperclip/approvals/{approval}/decide"), &json!({ "approve": true })).await.unwrap();
        self.tick().await; // authoritative read
        self.release(release.content_item_id, release.id).await
    }

    async fn release(&self, content: Uuid, id: Uuid) -> ReleaseRevision {
        self.op.content_detail(content).await.unwrap().releases.into_iter().find(|r| r.id == id).unwrap()
    }

    async fn ready_release(&self, accounts: &[&str]) -> (ContentItem, ReleaseRevision) {
        let item = self.brief(None).await;
        self.produce(item.id, 1).await;
        self.select_all(item.id).await;
        let rel = self.freeze(item.id, accounts, "The kiln is lit.").await;
        let rel = self.approve(&rel).await;
        assert_eq!(rel.state, ReleaseState::Approved);
        assert_eq!(self.op.content_detail(item.id).await.unwrap().item.state, ContentState::Approved);
        (item, rel)
    }

    async fn armed(&self) -> Vec<domain::ports::PublicationTimer> {
        self.tick().await;
        self.fx.windmill.as_ref().unwrap().armed_timers()
    }

    async fn fire(&self, t: &domain::ports::PublicationTimer) -> Result<DispatchOut, ClientError> {
        self.fx.windmill.as_ref().unwrap().fire(t).await
    }
}

#[tokio::test]
async fn complete_fixture_journey() {
    let Some(h) = harness().await else { return };
    // News arrives twice (sender retry) and from a second feed.
    let a = h.news("n1", "https://www.example.com/kiln?utm_source=x").await;
    let b = h.news("n1", "https://www.example.com/kiln?utm_source=x").await;
    assert!(a.created && !b.created && a.id == b.id);
    let n = h.op.news().await.unwrap().remove(0);
    assert_eq!(n.coverage, 2);
    assert_eq!(n.evidence.len(), 1);

    // Brief → Paperclip issue (fixture) linked.
    let item = h.brief(Some(n.id)).await;
    h.tick().await;
    let detail = h.op.content_detail(item.id).await.unwrap();
    assert!(detail.item.paperclip_issue_id.is_some(), "issue linked");

    // Generation → Windmill procedure → ComfyUI → receipts → import.
    let g = h.produce(item.id, 2).await;
    assert_eq!(g.state, GenerationState::Succeeded, "{g:?}");
    let detail = h.op.content_detail(item.id).await.unwrap();
    assert_eq!(detail.assets.iter().filter(|a| a.state == AssetState::Ready).count(), 2);
    let root = g.attempts.iter().find(|a| a.kind == AttemptKind::WindmillRoot).expect("root execution tracked");
    assert_eq!(root.state, AttemptState::Succeeded, "procedure followed to its end state");
    assert!(
        g.attempts.iter().filter(|a| a.kind == AttemptKind::ComfyPrompt).all(|a| a.parent_id == Some(root.id)),
        "prompt receipts linked to the root job"
    );
    let asset = &detail.assets[0];
    assert!(asset.provenance["prompt_id"].is_string() && asset.provenance["workflow"]["graph_sha256"].is_string());
    let (_, bytes) = h.op.get_bytes(&format!("/assets/{}/content", asset.id)).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("FIXTURE"));

    // Freeze without alt text is refused; then select, freeze, approve.
    h.op.select_asset(asset.id, &SelectAssetIn { selected: true, alt_text: None }).await.unwrap();
    let err =
        h.op.freeze_release(
            item.id,
            &FreezeReleaseIn {
                account_ids: vec!["fixture-mastodon".into()],
                text: "x".into(),
                platform_settings: json!({}),
                reply_to: None,
                timing: TimingScope { not_before: Utc::now(), not_after: Utc::now() + chrono::Duration::hours(1) },
            },
        )
        .await
        .unwrap_err();
    assert_eq!(code(err), "missing_alt_text");
    h.select_all(item.id).await;
    let rel = h.freeze(item.id, &["fixture-mastodon"], "The kiln is lit.").await;
    assert!(rel.payload_hash.starts_with("hub-release-v1:"));
    // Agents cannot approve or schedule.
    assert_eq!(code(h.agent.request_review(rel.id).await.unwrap_err()), "forbidden");
    let rel = h.approve(&rel).await;
    assert_eq!(rel.state, ReleaseState::Approved);
    assert!(rel.approved_by.as_deref().unwrap().starts_with("paperclip:fixture-board:"), "human attribution from Paperclip");

    // Windmill timer → guarded dispatch → published; duplicate timer converges.
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let timers = h.armed().await;
    assert_eq!(timers.len(), 1);
    let out = h.fire(&timers[0]).await.unwrap();
    assert!(out.submitted);
    assert_eq!(out.intent.state, IntentState::Published);
    let again = h.fire(&timers[0]).await.unwrap();
    assert!(!again.submitted);
    assert_eq!(h.fx.publisher.as_ref().unwrap().submissions().len(), 1);
    assert_eq!(h.op.content_detail(item.id).await.unwrap().item.state, ContentState::Published);

    // Engagement: inbound (delivered twice) → agent draft → review → reply.
    let ev = InteractionIn {
        account_id: "fixture-mastodon".into(),
        external_thread_id: "t1".into(),
        external_id: "m1".into(),
        direction: Direction::Inbound,
        author: "@fan".into(),
        body: "Open this weekend?".into(),
        occurred_at: Utc::now(),
        permalink: Some("https://fixture.social/@fan/1".into()),
        raw: json!({}),
    };
    let r1: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    let r2: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    assert!(r1.created && !r2.created);
    let d = h.op.relationship(r1.id).await.unwrap();
    assert_eq!(d.interactions.len(), 1);
    let draft = h
        .agent
        .draft_reply(
            r1.id,
            &DraftReplyIn {
                in_reply_to: Some(d.interactions[0].id),
                body: "Yes, Saturday 10–4!".into(),
                owner_epoch: d.relationship.owner_epoch,
            },
        )
        .await
        .unwrap();
    assert_eq!(code(h.agent.decide_draft(draft.id, &DraftDecisionIn::Approve).await.unwrap_err()), "forbidden");
    h.op.decide_draft(draft.id, &DraftDecisionIn::Approve).await.unwrap();
    h.tick().await;
    let d = h.op.relationship(r1.id).await.unwrap();
    assert_eq!(d.drafts[0].state, DraftState::Sent);
    assert!(d.interactions.iter().any(|i| i.direction == Direction::Outbound));
    // Handoff to the native platform.
    let ho = h.op.handoff(r1.id).await.unwrap();
    assert_eq!(ho.url, "https://fixture.social/@fan/1");

    // History carries correlation ids.
    let hist = h.op.content_detail(item.id).await.unwrap().history;
    assert!(hist.iter().any(|e| e.action == "release.approved"));
    assert!(hist.iter().filter(|e| e.actor.starts_with("operator")).all(|e| e.correlation_id.is_some()));
    h.server.abort();
}

#[tokio::test]
async fn changed_content_cannot_reuse_old_approval() {
    let Some(h) = harness().await else { return };
    let (item, r1) = h.ready_release(&["fixture-mastodon"]).await;
    let r2 = h.freeze(item.id, &["fixture-mastodon"], "The kiln is lit! (edited)").await;
    assert_ne!(r1.payload_hash, r2.payload_hash);
    assert_eq!(h.release(item.id, r1.id).await.state, ReleaseState::Superseded);
    let err =
        h.op.post::<_, ReleaseRevision>(
            &format!("/releases/{}/link-approval", r2.id),
            &LinkApprovalIn { approval_id: r1.paperclip_approval_id.clone().unwrap() },
        )
        .await
        .unwrap_err();
    assert_eq!(code(err), "approval_payload_mismatch");
    assert_eq!(code(h.op.schedule(r1.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap_err()), "release_not_approved");
    assert_eq!(code(h.op.schedule(r2.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap_err()), "release_not_approved");
}

#[tokio::test]
async fn stale_and_duplicate_timers_and_timing_scope() {
    let Some(h) = harness().await else { return };
    let (_, rel) = h.ready_release(&["fixture-mastodon"]).await;
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() + chrono::Duration::minutes(30) }).await.unwrap();
    // Reschedule within the approved scope: new schedule revision, no review.
    let now = Utc::now();
    let intents = h.op.schedule(rel.id, &ScheduleIn { due_at: now }).await.unwrap();
    assert_eq!(intents[0].schedule_revision, 2);
    let timers = h.armed().await;
    assert_eq!(timers.len(), 2);
    assert_eq!(code(h.fire(&timers[0]).await.unwrap_err()), "stale_schedule_revision");
    // Outside the window that was approved: needs a new revision.
    assert_eq!(
        code(h.op.schedule(rel.id, &ScheduleIn { due_at: now + chrono::Duration::days(3) }).await.unwrap_err()),
        "outside_timing_scope"
    );
    // Two current timers race; exactly one submits.
    let (a, b) = tokio::join!(h.fire(&timers[1]), h.fire(&timers[1]));
    let submitted = [a.unwrap().submitted, b.unwrap().submitted];
    assert_eq!(submitted.iter().filter(|s| **s).count(), 1, "{submitted:?}");
    assert_eq!(h.fx.publisher.as_ref().unwrap().submissions().len(), 1);
}

#[tokio::test]
async fn early_timer_pause_and_unverifiable_approval_fail_closed() {
    let Some(h) = harness().await else { return };
    let (_, rel) = h.ready_release(&["fixture-mastodon"]).await;
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() + chrono::Duration::hours(2) }).await.unwrap();
    let t = h.armed().await.remove(0);
    assert_eq!(code(h.fire(&t).await.unwrap_err()), "outside_dispatch_window");
    let t2 = h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let t2 = h.armed().await.into_iter().find(|t| t.schedule_revision == t2[0].schedule_revision).unwrap();
    h.op.set_pause(&PauseIn { paused: true, reason: Some("test".into()) }).await.unwrap();
    assert_eq!(code(h.fire(&t2).await.unwrap_err()), "production_paused");
    h.op.set_pause(&PauseIn { paused: false, reason: None }).await.unwrap();
    h.fx.paperclip.as_ref().unwrap().unreachable.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(code(h.fire(&t2).await.unwrap_err()), "approval_unverifiable");
    assert!(h.fx.publisher.as_ref().unwrap().submissions().is_empty());
    h.fx.paperclip.as_ref().unwrap().unreachable.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(h.fire(&t2).await.unwrap().submitted, "intent stayed scheduled after refusals");
    // Only the procedure role may dispatch.
    let err =
        h.op.post::<_, DispatchOut>(
            &format!("/intents/{}/dispatch", t2.intent_id),
            &DispatchIn { schedule_revision: t2.schedule_revision, trigger_id: "x".into() },
        )
        .await
        .unwrap_err();
    assert_eq!(code(err), "forbidden");
}

#[tokio::test]
async fn partial_multi_account_failure() {
    let Some(h) = harness().await else { return };
    h.fx.publisher.as_ref().unwrap().set_mode("fixture-bluesky", PublishMode::Reject);
    let (item, rel) = h.ready_release(&["fixture-mastodon", "fixture-bluesky"]).await;
    let intents = h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    assert_eq!(intents.len(), 2, "one intent per destination account");
    for t in h.armed().await {
        h.fire(&t).await.unwrap();
    }
    let detail = h.op.content_detail(item.id).await.unwrap();
    let state = |acct: &str| detail.intents.iter().find(|i| i.account_id == acct).unwrap().state;
    assert_eq!(state("fixture-mastodon"), IntentState::Published);
    assert_eq!(state("fixture-bluesky"), IntentState::Failed);
    assert_ne!(detail.item.state, ContentState::Published);
    assert!(h.op.today().await.unwrap().failed_operations.iter().any(|t| t.status == "failed"));
}

#[tokio::test]
async fn lost_publish_response_is_reconciled_not_resubmitted() {
    let Some(h) = harness().await else { return };
    h.fx.publisher.as_ref().unwrap().set_mode("fixture-mastodon", PublishMode::TimeoutAfterSend);
    let (_, rel) = h.ready_release(&["fixture-mastodon"]).await;
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let t = h.armed().await.remove(0);
    let out = h.fire(&t).await.unwrap();
    assert_eq!(out.intent.state, IntentState::OutcomeUnknown);
    // Windmill retry of the same timer must not resubmit.
    assert!(!h.fire(&t).await.unwrap().submitted);
    assert_eq!(code(h.op.cancel_intent(out.intent.id).await.unwrap_err()), "submission_started");
    h.tick().await;
    let intent = h.op.intents().await.unwrap().into_iter().find(|i| i.id == out.intent.id).unwrap();
    assert_eq!(intent.state, IntentState::Published);
    assert_eq!(h.fx.publisher.as_ref().unwrap().submissions().len(), 1);
}

#[tokio::test]
async fn accepted_is_tracked_until_confirmed_and_unclaimed_cancel_prevents_dispatch() {
    let Some(h) = harness().await else { return };
    h.fx.publisher.as_ref().unwrap().set_mode("fixture-mastodon", PublishMode::AcceptThenPublish);
    let (_, rel) = h.ready_release(&["fixture-mastodon", "fixture-bluesky"]).await;
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let timers = h.armed().await;
    let intents = h.op.intents().await.unwrap();
    let blue = intents.iter().find(|i| i.account_id == "fixture-bluesky").unwrap();
    h.op.cancel_intent(blue.id).await.unwrap();
    for t in &timers {
        let r = h.fire(t).await;
        if t.intent_id == blue.id.to_string() {
            assert_eq!(code(r.unwrap_err()), "intent_cancelled");
        } else {
            assert_eq!(r.unwrap().intent.state, IntentState::Accepted);
        }
    }
    h.tick().await;
    let masto = h.op.intents().await.unwrap().into_iter().find(|i| i.account_id == "fixture-mastodon").unwrap();
    assert_eq!(masto.state, IntentState::Published);
    assert_eq!(h.fx.publisher.as_ref().unwrap().submissions().len(), 1);
}

#[tokio::test]
async fn lost_generation_receipt_is_reconciled_by_known_prompt_id() {
    let Some(h) = harness().await else { return };
    h.fx.windmill.as_ref().unwrap().drop_receipts.store(true, std::sync::atomic::Ordering::SeqCst);
    let item = h.brief(None).await;
    let g = h.produce(item.id, 2).await;
    assert_eq!(g.state, GenerationState::Succeeded, "{g:?}");
    assert_eq!(h.op.content_detail(item.id).await.unwrap().assets.len(), 2);
}

#[tokio::test]
async fn targeted_cancel_leaves_unrelated_work_running() {
    let Some(h) = harness().await else { return };
    let comfy = h.fx.comfy.clone().unwrap();
    comfy.submit("unrelated-job", "someone-else", json!({}));
    comfy.tick();
    assert_eq!(comfy.running().as_deref(), Some("unrelated-job"));
    let item = h.brief(None).await;
    let wf = h.workflow("poster-txt2img").await;
    let g = h.op.generate(item.id, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "x" }), variants: 2 }).await.unwrap();
    h.tick().await;
    h.wait_queued(3).await;
    let out = h.op.cancel_generation(g.id).await.unwrap();
    assert_eq!(out.state, GenerationState::Cancelled, "{out:?}");
    assert_eq!(comfy.running().as_deref(), Some("unrelated-job"), "unrelated job still running");
    assert!(comfy.queued().is_empty());
}

#[tokio::test]
async fn human_takeover_beats_queued_agent_reply() {
    let Some(h) = harness().await else { return };
    let ev = InteractionIn {
        account_id: "fixture-mastodon".into(),
        external_thread_id: "t9".into(),
        external_id: "m9".into(),
        direction: Direction::Inbound,
        author: "@critic".into(),
        body: "This is wrong.".into(),
        occurred_at: Utc::now(),
        permalink: None,
        raw: json!({}),
    };
    let r: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    let draft = h.agent.draft_reply(r.id, &DraftReplyIn { in_reply_to: None, body: "Thanks!".into(), owner_epoch: 1 }).await.unwrap();
    h.op.decide_draft(draft.id, &DraftDecisionIn::Approve).await.unwrap(); // queued in the outbox
    let rel = h.op.takeover(r.id).await.unwrap(); // races the worker
    assert_eq!(rel.owner, Owner::Human);
    h.tick().await;
    assert!(h.fx.publisher.as_ref().unwrap().reply_submissions().is_empty(), "agent reply must not be sent");
    let d = h.op.relationship(r.id).await.unwrap();
    assert_eq!(d.drafts[0].state, DraftState::Invalidated);
    // The agent can no longer draft; a stale epoch is refused even for humans.
    assert_eq!(
        code(
            h.agent
                .draft_reply(r.id, &DraftReplyIn { in_reply_to: None, body: "x".into(), owner_epoch: rel.owner_epoch })
                .await
                .unwrap_err()
        ),
        "human_owns_conversation"
    );
    assert_eq!(
        code(h.op.draft_reply(r.id, &DraftReplyIn { in_reply_to: None, body: "x".into(), owner_epoch: 1 }).await.unwrap_err()),
        "stale_owner_epoch"
    );
}

#[tokio::test]
async fn assets_survive_engine_history_cleanup_and_restart() {
    let Some(h) = harness().await else { return };
    let item = h.brief(None).await;
    let wf = h.workflow("poster-txt2img").await;
    let g = h.op.generate(item.id, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "y" }), variants: 1 }).await.unwrap();
    h.tick().await;
    h.wait_queued(1).await;
    // Hub "restarts" mid-production with no progress socket: state is durable.
    let comfy = h.fx.comfy.clone().unwrap();
    for _ in 0..(comfy.steps + 1) {
        comfy.tick();
    }
    h.server.abort();
    let restarted = start(&h.db_url, &h.assets).await;
    // Same engine state (the GPU kept running while the hub was down).
    let hub = domain::Hub { comfy: comfy.clone(), ..restarted.hub.clone() };
    worker::tick(&hub).await.unwrap();
    let detail = restarted.op.content_detail(item.id).await.unwrap();
    assert_eq!(detail.generations.iter().find(|x| x.id == g.id).unwrap().state, GenerationState::Succeeded);
    let asset = detail.assets[0].clone();
    comfy.clear_history();
    worker::tick(&hub).await.unwrap();
    let (_, bytes) = restarted.op.get_bytes(&format!("/assets/{}/content", asset.id)).await.unwrap();
    assert_eq!(domain::hash::sha256_hex(&bytes), asset.sha256);
    assert_eq!(restarted.op.content_detail(item.id).await.unwrap().assets[0].provenance["prompt_id"], asset.provenance["prompt_id"]);
}

#[tokio::test]
async fn chat_suggestions_are_proposals_not_canon() {
    let Some(h) = harness().await else { return };
    let p = h.persona().await;
    let before = p.canon.clone().unwrap();
    let msgs = h.op.send_chat("persona", p.id, &ChatMessageIn { body: "rule: never post before 9am".into() }).await.unwrap();
    let proposal = msgs.iter().find_map(|m| m.proposal_id).expect("proposal attached");
    let p2 = h.persona().await;
    assert_eq!(p2.canon.unwrap().id, before.id, "accepted canon unchanged");
    assert_eq!(p2.pending_proposals, 1);
    assert_eq!(code(h.agent.decide_canon(proposal, &CanonDecisionIn::Accept).await.unwrap_err()), "forbidden");
    let accepted = h.op.decide_canon(proposal, &CanonDecisionIn::Accept).await.unwrap();
    assert!(accepted.body.rules.iter().any(|r| r.contains("9am")));
    // A second proposal written against the old base is stale.
    let stale = h
        .agent
        .propose_canon(
            p.id,
            &CanonProposalIn {
                base_revision: Some(before.revision),
                body: before.body.clone(),
                source: CanonSource::Operator,
                rationale: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(stale.source, CanonSource::Agent, "agents cannot claim operator authorship");
    assert_eq!(code(h.op.decide_canon(stale.id, &CanonDecisionIn::Accept).await.unwrap_err()), "stale_canon_base");
    // Procedure tokens cannot read product data.
    assert_eq!(code(h.procedure.news().await.unwrap_err()), "forbidden");
}

#[test]
fn openapi_lists_every_route() {
    let doc: serde_json::Value = serde_json::from_str(fleetix_hubd::OPENAPI).unwrap();
    let src = include_str!("../src/lib.rs");
    for line in src.lines().filter(|l| l.trim_start().starts_with(".route(\"")) {
        let path = line.split('"').nth(1).unwrap();
        let full = if path.starts_with("/openapi") || path == "/healthz" { path.to_string() } else { format!("/v1{path}") };
        assert!(doc["paths"].get(&full).is_some(), "openapi.json misses {full}");
    }
}

#[tokio::test]
async fn gpu_admission_treats_unknown_jobs_and_loaded_llm_as_busy() {
    let Some(h) = harness().await else { return };
    domain::production::register_engine(
        &h.hub,
        "comfy-atlas",
        "fixture://comfyui",
        json!({ "admission": { "max_in_flight": 1, "defer_on_unknown_work": true, "defer_while_llm_loaded": true, "llm_running_url": "http://llama-swap/running" } }),
    )
    .await
    .unwrap();
    let comfy = h.fx.comfy.clone().unwrap();
    let peer = h.fx.gpu_peer.clone().unwrap();
    let release_outbox = || async {
        domain::sqlx::query("update outbox set next_attempt_at = now() where state = 'pending'").execute(&h.hub.db).await.unwrap();
    };
    comfy.submit("someone-elses-job", "other-client", json!({}));
    let item = h.brief(None).await;
    let wf = h.workflow("poster-txt2img").await;
    let g = h.op.generate(item.id, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "z" }), variants: 1 }).await.unwrap();
    h.tick().await;
    assert_eq!(comfy.queued(), vec!["someone-elses-job".to_string()], "not submitted while unknown work is queued");
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Requested);
    let hist = h.op.get::<Vec<HistoryEntry>>(&format!("/history/generation/{}", g.id)).await.unwrap();
    assert!(hist.iter().any(|e| e.action == "generation.awaiting_admission"));
    // Unknown job finishes, but an LLM now holds the shared GPU.
    for _ in 0..(comfy.steps + 1) {
        comfy.tick();
    }
    peer.loaded.store(true, std::sync::atomic::Ordering::SeqCst);
    release_outbox().await;
    // A long deferral is not a lost submission: prompts stay open.
    domain::sqlx::query("update execution_attempts set created_at = now() - interval '1 hour' where generation_request_id = $1")
        .bind(g.id)
        .execute(&h.hub.db)
        .await
        .unwrap();
    h.tick().await;
    let deferred = h.generation(item.id, g.id).await;
    assert_eq!(deferred.state, GenerationState::Requested);
    assert!(deferred.attempts.iter().all(|a| a.state == AttemptState::Submitted), "{:?}", deferred.attempts);
    // Device free: admitted and dispatched; a deferral never spent a retry.
    peer.loaded.store(false, std::sync::atomic::Ordering::SeqCst);
    release_outbox().await;
    h.tick().await;
    let state = h.generation(item.id, g.id).await.state;
    assert!(matches!(state, GenerationState::Dispatched | GenerationState::Running), "{state:?}");
    let attempts: i32 = domain::sqlx::query_scalar("select attempts from outbox where topic = 'windmill.run_generation'")
        .fetch_one(&h.hub.db)
        .await
        .unwrap();
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn generation_cancelled_while_awaiting_admission_is_never_submitted() {
    let Some(h) = harness().await else { return };
    domain::production::register_engine(
        &h.hub,
        "comfy-atlas",
        "fixture://comfyui",
        json!({ "admission": { "defer_while_llm_loaded": true, "llm_running_url": "http://llama-swap/running" } }),
    )
    .await
    .unwrap();
    let comfy = h.fx.comfy.clone().unwrap();
    let peer = h.fx.gpu_peer.clone().unwrap();
    peer.loaded.store(true, std::sync::atomic::Ordering::SeqCst);
    let item = h.brief(None).await;
    let wf = h.workflow("poster-txt2img").await;
    let g = h.op.generate(item.id, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "z" }), variants: 1 }).await.unwrap();
    h.tick().await;
    h.op.cancel_generation(g.id).await.unwrap();
    peer.loaded.store(false, std::sync::atomic::Ordering::SeqCst);
    domain::sqlx::query("update outbox set next_attempt_at = now() where state = 'pending'").execute(&h.hub.db).await.unwrap();
    h.tick().await;
    assert!(comfy.queued().is_empty() && comfy.running().is_none(), "cancelled work reached the engine");
    let state: String =
        domain::sqlx::query_scalar("select state from outbox where topic = 'windmill.run_generation'").fetch_one(&h.hub.db).await.unwrap();
    assert_eq!(state, "dead");
}

#[tokio::test]
async fn unreachable_publisher_arms_a_retry_timer_for_the_same_revision() {
    let Some(h) = harness().await else { return };
    let publisher = h.fx.publisher.clone().unwrap();
    publisher.set_mode("fixture-mastodon", PublishMode::Unreachable);
    let (_, rel) = h.ready_release(&["fixture-mastodon"]).await;
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let first = h.armed().await.remove(0);
    assert_eq!(h.fire(&first).await.unwrap().intent.state, IntentState::Scheduled);
    let timers = h.armed().await;
    assert_eq!(timers.len(), 2, "a retry timer was armed");
    let retry = timers.last().unwrap().clone();
    assert_eq!((retry.intent_id.as_str(), retry.schedule_revision), (first.intent_id.as_str(), first.schedule_revision));
    publisher.set_mode("fixture-mastodon", PublishMode::Publish);
    assert_eq!(h.fire(&retry).await.unwrap().intent.state, IntentState::Published);
}

fn actor(role: Role, name: &str) -> domain::Actor {
    domain::Actor { principal: Principal { id: Uuid::new_v4(), name: name.into(), role }, correlation_id: None }
}

fn timer_trigger() -> domain::Actor {
    actor(Role::Procedure, "timer:test")
}

/// A publishing transport whose reconciliation read always reports a pending
/// claim with no platform result, as n8n contract v1 does: the provider took
/// the request, the hub never saw the response.
struct PendingLookup;

#[async_trait::async_trait]
impl domain::ports::Publisher for PendingLookup {
    async fn submit(&self, _: &domain::ports::PublishRequest) -> domain::ports::PortResult<domain::ports::ProviderPost> {
        panic!("a crashed submission must never be resubmitted");
    }
    async fn lookup(&self, _: &str, key: &str) -> domain::ports::PortResult<Option<domain::ports::ProviderPost>> {
        Err(domain::ports::PortError::Ambiguous(format!("claim {key} is pending")))
    }
    async fn reply(&self, _: &domain::ports::ReplyRequest) -> domain::ports::PortResult<domain::ports::ProviderPost> {
        panic!("no reply is expected in this test");
    }
    async fn lookup_reply(&self, _: &str, _: &str) -> domain::ports::PortResult<Option<domain::ports::ProviderPost>> {
        Ok(None)
    }
}

impl H {
    async fn add_account(&self, id: &str, transport: &str) {
        domain::system::upsert_account(
            &self.hub,
            &Account {
                id: id.into(),
                company_id: "fixture-company".into(),
                platform: "plinth".into(),
                handle: format!("@{id}"),
                transport: transport.into(),
                persona_id: None,
                capabilities: AccountCapabilities { publish: true, replies: true, ..Default::default() },
            },
        )
        .await
        .unwrap();
    }

    /// Freeze → approve → schedule, returning the single armed timer.
    async fn armed_release(&self, content: Uuid, account: &str) -> domain::ports::PublicationTimer {
        let rel = self.freeze(content, &[account], "The kiln is lit.").await;
        let rel = self.approve(&rel).await;
        self.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
        self.armed().await.remove(0)
    }
}

#[tokio::test]
async fn dispatch_uses_the_frozen_transport_channel() {
    let Some(h) = harness().await else { return };
    h.add_account("test-blog", "plinth").await;
    let item = h.brief(None).await;
    h.produce(item.id, 1).await;
    h.select_all(item.id).await;
    let rel = h.freeze(item.id, &["test-blog"], "On the blog.").await;
    assert_eq!(rel.actions[0].transport.as_deref(), Some("plinth"), "the account's transport is frozen into the payload");
    let rel = h.approve(&rel).await;
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let timer = h.armed().await.remove(0);
    // Only this test's hub knows the `plinth` channel; the server keeps the
    // built-in one.
    let plinth = Arc::new(FixturePublisher::default());
    let mut channels = (*h.hub.channels).clone();
    channels.insert("plinth".into(), plinth.clone() as Arc<dyn domain::ports::Publisher>);
    let hub = domain::Hub { channels: Arc::new(channels), ..h.hub.clone() };
    let out = domain::release::dispatch(
        &hub,
        &timer_trigger(),
        Uuid::parse_str(&timer.intent_id).unwrap(),
        DispatchIn { schedule_revision: timer.schedule_revision, trigger_id: "t".into() },
    )
    .await
    .unwrap();
    assert_eq!(out.intent.state, IntentState::Published);
    assert_eq!(plinth.submissions().len(), 1, "the frozen transport carried the post");
    assert_eq!(plinth.submissions()[0].account_id, "test-blog");
    assert!(h.fx.publisher.as_ref().unwrap().submissions().is_empty(), "the built-in channel was not used");
}

#[tokio::test]
async fn a_missing_frozen_channel_fails_closed_without_fallback() {
    let Some(h) = harness().await else { return };
    h.add_account("test-orphan", "nowhere").await;
    let item = h.brief(None).await;
    h.produce(item.id, 1).await;
    h.select_all(item.id).await;
    let timer = h.armed_release(item.id, "test-orphan").await;
    let out = domain::release::dispatch(
        &h.hub,
        &timer_trigger(),
        Uuid::parse_str(&timer.intent_id).unwrap(),
        DispatchIn { schedule_revision: timer.schedule_revision, trigger_id: "t".into() },
    )
    .await
    .unwrap();
    assert!(!out.submitted);
    assert_eq!(out.intent.state, IntentState::Failed, "a missing channel needs an operator, never a fallback");
    assert!(h.fx.publisher.as_ref().unwrap().submissions().is_empty());
    let hist: Vec<HistoryEntry> = h.op.get(&format!("/history/intent/{}", out.intent.id)).await.unwrap();
    assert!(hist.iter().any(|e| e.action == "intent.delivery" && e.detail["detail"]["code"] == "transport_unavailable"), "{hist:?}");
}

#[tokio::test]
async fn a_crash_after_the_claim_is_never_double_published() {
    let Some(h) = harness().await else { return };
    let (_, rel) = h.ready_release(&["fixture-mastodon"]).await;
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let timer = h.armed().await.remove(0);
    let intent = Uuid::parse_str(&timer.intent_id).unwrap();
    let key = format!("ihub-{intent}-1");
    // The hub dies after the claim commit and before the submit call: the
    // attempt row and the claimed intent are durable, nothing was sent.
    domain::sqlx::query(
        "insert into delivery_attempts (id, intent_id, attempt, idempotency_key, trigger_id, state) values ($1, $2, 1, $3, 'crashed', 'submitting')",
    )
    .bind(Uuid::new_v4())
    .bind(intent)
    .bind(&key)
    .execute(&h.hub.db)
    .await
    .unwrap();
    domain::sqlx::query("update publication_intents set state = 'submitting', claimed_by = 'crashed', claimed_at = now() - interval '5 minutes' where id = $1")
        .bind(intent)
        .execute(&h.hub.db)
        .await
        .unwrap();
    // A duplicate timer converges on the claim instead of submitting again.
    assert!(!h.fire(&timer).await.unwrap().submitted);
    assert!(h.fx.publisher.as_ref().unwrap().submissions().is_empty());
    // Reconciliation finds a pending provider record for exactly this attempt:
    // whether the post exists is unknown, so the intent stops for an operator.
    // Reconciliation reads through the frozen transport's channel.
    let pending: Arc<dyn domain::ports::Publisher> = Arc::new(PendingLookup);
    let channels = h.hub.channels.keys().map(|k| (k.clone(), pending.clone())).collect();
    let hub = domain::Hub { publisher: pending, channels: Arc::new(channels), ..h.hub.clone() };
    worker::reconcile(&hub).await.unwrap();
    let crashed = h.op.intents().await.unwrap().into_iter().find(|i| i.id == intent).unwrap();
    assert_eq!(crashed.state, IntentState::OutcomeUnknown);
    assert_eq!(crashed.attempts.last().unwrap().idempotency_key, key);
    // The operator confirms nothing was posted and reschedules: one attempt,
    // one post, no duplicate.
    h.op.post::<_, PublicationIntent>(
        &format!("/intents/{intent}/resolve"),
        &ResolveIntentIn { published: false, provider_post_id: None, provider_url: None },
    )
    .await
    .unwrap();
    h.op.schedule(rel.id, &ScheduleIn { due_at: Utc::now() }).await.unwrap();
    let retry = h.armed().await.into_iter().find(|t| t.schedule_revision > timer.schedule_revision).unwrap();
    assert_eq!(h.fire(&retry).await.unwrap().intent.state, IntentState::Published);
    assert_eq!(h.fx.publisher.as_ref().unwrap().submissions().len(), 1);
}

#[tokio::test]
async fn producer_decides_who_starts_the_work_and_the_hub_always_publishes() {
    let Some(h) = harness().await else { return };
    domain::sqlx::query(
        "insert into settings (key, value) values ('paperclip_studio', $1) on conflict (key) do update set value = excluded.value",
    )
    .bind(json!({ "project_id": "project_studio_content", "assignee_agent_id": "agent_studio_showrunner" }))
    .execute(&h.hub.db)
    .await
    .unwrap();
    // Delegated: the studio org gets the issue, with the project and the
    // assignee that make the task bridge start work.
    let delegated = h.brief_with(None, Producer::Delegated).await;
    h.tick().await;
    assert!(h.op.content_detail(delegated.id).await.unwrap().item.paperclip_issue_id.is_some(), "issue linked");
    let req =
        h.fx.paperclip
            .as_ref()
            .unwrap()
            .issue_requests()
            .into_iter()
            .find(|r| r.origin_key == format!("fleetix-hub:content:{}", delegated.id))
            .unwrap();
    assert_eq!(req.project_id.as_deref(), Some("project_studio_content"));
    assert_eq!(req.assignee_agent_id.as_deref(), Some("agent_studio_showrunner"));
    // Direct: the hub runs the pipeline; no issue is filed.
    let direct = h.brief_with(None, Producer::Direct).await;
    h.tick().await;
    let detail = h.op.content_detail(direct.id).await.unwrap();
    assert!(detail.item.paperclip_issue_id.is_none());
    assert_eq!(detail.item.brief.producer, Producer::Direct);
    assert!(
        h.fx.paperclip.as_ref().unwrap().issue_requests().iter().all(|r| r.origin_key != format!("fleetix-hub:content:{}", direct.id)),
        "a direct brief must not file a Paperclip issue"
    );
    // The hub still owns publication for a direct brief.
    let g = h.produce(direct.id, 1).await;
    assert_eq!(g.state, GenerationState::Succeeded);
    h.select_all(direct.id).await;
    let timer = h.armed_release(direct.id, "fixture-mastodon").await;
    assert_eq!(h.fire(&timer).await.unwrap().intent.state, IntentState::Published);
}

#[tokio::test]
async fn operator_asks_the_assistant_for_a_reply_draft() {
    let Some(h) = harness().await else { return };
    let ev = InteractionIn {
        account_id: "fixture-mastodon".into(),
        external_thread_id: "t-reply".into(),
        external_id: "m-reply".into(),
        direction: Direction::Inbound,
        author: "@fan".into(),
        body: "Open this weekend?".into(),
        occurred_at: Utc::now(),
        permalink: Some("https://fixture.social/@fan/9".into()),
        raw: json!({}),
    };
    let r: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    let d = h.op.relationship(r.id).await.unwrap();
    // Only the operator drives assistant drafting; the public has no chat.
    let err = domain::engagement::draft_assistant_reply(&h.hub, &actor(Role::Agent, "agent:test"), r.id).await.unwrap_err();
    assert_eq!(err.code(), "forbidden");
    let draft = domain::engagement::draft_assistant_reply(&h.hub, &actor(Role::Operator, "operator:test"), r.id).await.unwrap();
    assert_eq!(draft.state, DraftState::InReview);
    assert_eq!(draft.author_kind, Owner::Agent);
    assert!(draft.author.starts_with("assistant:"), "{}", draft.author);
    assert_eq!(draft.owner_epoch, d.relationship.owner_epoch);
    assert_eq!(draft.in_reply_to, Some(d.interactions[0].id));
    assert!(!draft.body.trim().is_empty());
    // The operator approves; the channel sends the approved text.
    h.op.decide_draft(draft.id, &DraftDecisionIn::Approve).await.unwrap();
    h.tick().await;
    let sent = h.op.relationship(r.id).await.unwrap();
    assert_eq!(sent.drafts[0].state, DraftState::Sent);
    let replies = h.fx.publisher.as_ref().unwrap().reply_submissions();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].text, draft.body);
    // A human takeover stops assistant drafting.
    h.op.takeover(r.id).await.unwrap();
    let err = domain::engagement::draft_assistant_reply(&h.hub, &actor(Role::Operator, "operator:test"), r.id).await.unwrap_err();
    assert_eq!(err.code(), "human_owns_conversation");
}

/// An assistant that only answers once the test releases it, so ownership can
/// move while a draft is being written.
struct GatedAssistant {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    gate: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl domain::ports::Assistant for GatedAssistant {
    async fn reply(
        &self,
        _: &serde_json::Value,
        _: &[domain::ports::AssistantTurn],
    ) -> domain::ports::PortResult<domain::ports::AssistantReply> {
        let _ = self.entered.send(());
        self.gate.notified().await;
        Ok(domain::ports::AssistantReply { body: "A draft from the old epoch".into(), canon_suggestion: None, model: "gated".into() })
    }
}

#[tokio::test]
async fn takeover_and_release_during_assistant_drafting_fails_with_owner_changed() {
    let Some(h) = harness().await else { return };
    let ev = InteractionIn {
        account_id: "fixture-mastodon".into(),
        external_thread_id: "t-churn".into(),
        external_id: "m-churn".into(),
        direction: Direction::Inbound,
        author: "@fan".into(),
        body: "Still there?".into(),
        occurred_at: Utc::now(),
        permalink: None,
        raw: json!({}),
    };
    let r: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Notify::new());
    let hub = domain::Hub { assistant: Arc::new(GatedAssistant { entered, gate: gate.clone() }), ..h.hub.clone() };
    let operator = actor(Role::Operator, "operator:test");
    let rel_id = r.id;
    let drafting = tokio::spawn(async move { domain::engagement::draft_assistant_reply(&hub, &operator, rel_id).await });
    entered_rx.recv().await.expect("the assistant is writing");
    // Epoch 1 → 2 → 3: the agent owns the conversation again, but not the
    // epoch the draft started under.
    h.op.takeover(r.id).await.unwrap();
    h.op.release_to_agent(r.id).await.unwrap();
    gate.notify_one();
    let err = drafting.await.unwrap().unwrap_err();
    assert_eq!(err.code(), "owner_changed");
    assert!(h.op.relationship(r.id).await.unwrap().drafts.is_empty(), "no draft is stamped with the new epoch");
}

#[tokio::test]
async fn a_reply_goes_through_the_accounts_registered_channel() {
    let Some(h) = harness().await else { return };
    h.add_account("test-reply", "plinth").await;
    let ev = InteractionIn {
        account_id: "test-reply".into(),
        external_thread_id: "t-channel".into(),
        external_id: "m-channel".into(),
        direction: Direction::Inbound,
        author: "@fan".into(),
        body: "Any news?".into(),
        occurred_at: Utc::now(),
        permalink: None,
        raw: json!({}),
    };
    let r: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    let d = h.op.relationship(r.id).await.unwrap();
    let draft = h
        .agent
        .draft_reply(
            r.id,
            &DraftReplyIn { in_reply_to: Some(d.interactions[0].id), body: "Soon!".into(), owner_epoch: d.relationship.owner_epoch },
        )
        .await
        .unwrap();
    h.op.decide_draft(draft.id, &DraftDecisionIn::Approve).await.unwrap();
    // Only this test's hub knows the `plinth` channel; the server keeps the
    // built-in one.
    let channel = Arc::new(FixturePublisher::default());
    let mut channels = (*h.hub.channels).clone();
    channels.insert("plinth".into(), channel.clone() as Arc<dyn domain::ports::Publisher>);
    let hub = domain::Hub { channels: Arc::new(channels), ..h.hub.clone() };
    let out = domain::engagement::deliver_reply(&hub, draft.id).await.unwrap();
    assert_eq!(out, domain::engagement::ReplyDelivery::Sent);
    let replies = channel.reply_submissions();
    assert_eq!(replies.len(), 1, "the account's registered channel carried the reply");
    assert_eq!(replies[0].idempotency_key, format!("ihub-reply-{}", draft.id));
    assert_eq!(replies[0].account_id, "test-reply");
    assert_eq!(replies[0].in_reply_to.as_deref(), Some("m-channel"));
    assert!(h.fx.publisher.as_ref().unwrap().reply_submissions().is_empty(), "the built-in publisher was not used");
}

#[tokio::test]
async fn a_reply_on_an_unregistered_transport_fails_closed() {
    let Some(h) = harness().await else { return };
    h.add_account("test-orphan-reply", "nowhere").await;
    let ev = InteractionIn {
        account_id: "test-orphan-reply".into(),
        external_thread_id: "t-orphan".into(),
        external_id: "m-orphan".into(),
        direction: Direction::Inbound,
        author: "@fan".into(),
        body: "Hello?".into(),
        occurred_at: Utc::now(),
        permalink: None,
        raw: json!({}),
    };
    let r: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    let draft = h.agent.draft_reply(r.id, &DraftReplyIn { in_reply_to: None, body: "Hello!".into(), owner_epoch: 1 }).await.unwrap();
    h.op.decide_draft(draft.id, &DraftDecisionIn::Approve).await.unwrap();
    match domain::engagement::deliver_reply(&h.hub, draft.id).await.unwrap() {
        domain::engagement::ReplyDelivery::Failed(message) => {
            assert!(message.contains("transport_unavailable"), "{message}")
        }
        other => panic!("a missing channel must fail the reply, got {other:?}"),
    }
    let d = h.op.relationship(r.id).await.unwrap();
    assert_eq!(d.drafts.iter().find(|x| x.id == draft.id).unwrap().state, DraftState::Rejected);
    assert!(h.fx.publisher.as_ref().unwrap().reply_submissions().is_empty(), "there is no fallback to the built-in publisher");
}

#[tokio::test]
async fn pipeline_generation_succeeds_only_when_every_manifest_artifact_is_uploaded() {
    let Some(h) = harness().await else { return };
    let item = h.brief(None).await;
    let wf = h.workflow("pipeline-svg").await;
    let g =
        h.op.generate(item.id, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "a kiln at dusk" }), variants: 1 })
            .await
            .unwrap();
    assert_eq!(g.state, GenerationState::Requested);
    assert!(
        g.attempts.iter().any(|a| a.kind == AttemptKind::PipelineRun),
        "the pipeline attempt is recorded before submission: {:?}",
        g.attempts
    );
    h.tick().await; // admission (no target engine) → submit through the fixture pipeline
    let mut state = g.state;
    for _ in 0..200 {
        state = h.generation(item.id, g.id).await.state;
        if state == GenerationState::Succeeded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(state, GenerationState::Succeeded);
    let detail = h.op.content_detail(item.id).await.unwrap();
    assert_eq!(detail.item.state, ContentState::Selecting);
    assert_eq!(detail.assets.iter().filter(|a| a.state == AssetState::Ready).count(), 2, "both manifest artifacts are durable");
    let missing: i64 =
        domain::sqlx::query_scalar("select count(*) from generation_artifacts where generation_request_id = $1 and asset_id is null")
            .bind(g.id)
            .fetch_one(&h.hub.db)
            .await
            .unwrap();
    assert_eq!(missing, 0, "every manifest row is linked to its uploaded asset");

    // The recorded hash is authoritative: bytes that do not match are refused.
    let upload = |source: &str, sha: String, body: &'static str| {
        let url = format!("{}/v1/generations/{}/artifacts", h.url, g.id);
        reqwest::Client::new()
            .post(url)
            .bearer_auth("pr")
            .header("x-artifact-source", source)
            .header("content-type", "text/plain")
            .header("x-sha256", sha)
            .body(body)
    };
    let resp = upload("render", "0".repeat(64), "not the recorded bytes").send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY, "a hash mismatch is rejected");
    assert_eq!(resp.json::<ApiError>().await.unwrap().code, "artifact_hash_mismatch");
    // Bytes that hash correctly but contradict the recorded artifact are a
    // state conflict.
    let other = domain::hash::sha256_hex(b"a different render");
    let resp = upload("render", other, "a different render").send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(resp.json::<ApiError>().await.unwrap().code, "artifact_hash_mismatch");
    // A source outside the recorded manifest cannot be added after completion.
    let sha = domain::hash::sha256_hex(b"late extra output");
    let resp = upload("extra", sha, "late extra output").send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(resp.json::<ApiError>().await.unwrap().code, "artifact_not_in_manifest");

    // The route takes bodies well past the router's 2 MiB JSON limit: the
    // bytes reach the handler (422 here), not a 413 from the extractor.
    let big = "x".repeat(3 * 1024 * 1024);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/generations/{}/artifacts", h.url, g.id))
        .bearer_auth("pr")
        .header("x-artifact-source", "render")
        .header("x-sha256", "0".repeat(64))
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY, "the artifact route raises the body limit");

    // A queued pipeline generation cancels without ever starting the pipeline.
    let pending =
        h.op.generate(item.id, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "second take" }), variants: 1 }).await.unwrap();
    let out = h.op.cancel_generation(pending.id).await.unwrap();
    assert_eq!(out.state, GenerationState::Cancelled);
    assert!(out.actions.iter().any(|a| a.contains("never submitted")), "{:?}", out.actions);
    h.tick().await;
    assert_eq!(h.generation(item.id, pending.id).await.state, GenerationState::Cancelled, "the dropped command never starts");
    h.server.abort();
}

// ── Pipeline terminal semantics ──────────────────────────────────────────────
//
// A generation succeeds only when a terminal manifest is recorded *and* every
// manifest artifact is uploaded with a matching digest. These tests drive the
// receipt/upload protocol by hand (`manual_pipeline`), so each half of the
// rule is exercised on its own.

impl H {
    /// Record the submission and nothing else: no uploads, no receipt.
    fn manual_pipeline(&self) {
        self.fx.windmill.as_ref().unwrap().manual_pipeline.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    async fn pipeline(&self, item: Uuid) -> GenerationRequest {
        let wf = self.workflow("pipeline-svg").await;
        self.op
            .generate(item, &GenerateIn { workflow_id: wf.id, inputs: json!({ "prompt": "a kiln at dusk" }), variants: 1 })
            .await
            .unwrap()
    }

    async fn pipeline_receipt(&self, id: Uuid, status: JobStatus, artifacts: Vec<ArtifactRef>) -> Result<GenerationRequest, ClientError> {
        self.procedure
            .post(
                &format!("/generations/{id}/receipts"),
                &ReceiptIn {
                    kind: AttemptKind::PipelineRun,
                    external_id: "fixture-run".into(),
                    parent_external_id: None,
                    engine_instance_id: None,
                    status: Some(status),
                    artifacts,
                    raw: json!({ "test": true }),
                },
            )
            .await
    }

    /// Raw body upload with the procedure token, as the pipeline posts it.
    async fn upload(&self, id: Uuid, source: &str, mime: &str, sha256: &str, bytes: &[u8]) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/generations/{id}/artifacts", self.url))
            .bearer_auth("pr")
            .header("x-artifact-source", source)
            .header("content-type", mime)
            .header("x-sha256", sha256)
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap()
    }

    async fn attempt(&self, content: Uuid, id: Uuid) -> ExecutionAttempt {
        self.generation(content, id).await.attempts.into_iter().find(|a| a.kind == AttemptKind::PipelineRun).unwrap()
    }

    async fn attempt_raw(&self, id: Uuid) -> serde_json::Value {
        domain::sqlx::query_scalar("select raw from execution_attempts where generation_request_id = $1 and kind = 'pipeline_run'")
            .bind(id)
            .fetch_one(&self.hub.db)
            .await
            .unwrap()
    }

    async fn unlinked(&self, id: Uuid) -> i64 {
        domain::sqlx::query_scalar("select count(*) from generation_artifacts where generation_request_id = $1 and asset_id is null")
            .bind(id)
            .fetch_one(&self.hub.db)
            .await
            .unwrap()
    }
}

/// A manifest with no upload can no longer complete by marking the run
/// terminal; it fails at the deadline and names what never arrived.
#[tokio::test]
async fn a_manifest_without_uploads_fails_the_generation_at_the_deadline() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.tick().await; // submission only
    let render = b"a render that never arrives";
    let caption = b"a caption that never arrives";
    let manifest = vec![
        ArtifactRef {
            source: "render".into(),
            sha256: domain::hash::sha256_hex(render),
            mime: "image/svg+xml".into(),
            size: render.len() as u64,
        },
        ArtifactRef {
            source: "caption".into(),
            sha256: domain::hash::sha256_hex(caption),
            mime: "text/plain".into(),
            size: caption.len() as u64,
        },
    ];
    let recorded = h.pipeline_receipt(g.id, JobStatus::Succeeded, manifest).await.unwrap();
    assert_eq!(recorded.state, GenerationState::Running, "a manifest alone does not complete the generation");
    assert_eq!(h.attempt(item.id, g.id).await.state, AttemptState::Running, "the attempt stays open for its uploads");

    // Age the recorded manifest past the upload deadline.
    domain::sqlx::query(
        "update execution_attempts set raw = raw || '{\"manifest_at\": 0}'::jsonb
          where generation_request_id = $1 and kind = 'pipeline_run'",
    )
    .bind(g.id)
    .execute(&h.hub.db)
    .await
    .unwrap();
    h.tick().await;
    let overdue = h.generation(item.id, g.id).await;
    assert_eq!(overdue.state, GenerationState::Failed, "missing uploads fail the generation");
    let attempt = h.attempt(item.id, g.id).await;
    assert_eq!(attempt.state, AttemptState::Failed);
    assert_eq!(attempt.progress["missing"], json!(["caption", "render"]), "the failure names the missing sources");
    h.server.abort();
}

/// Cancelling while an upload is missing must end the request now, not wait for
/// the deadline or succeed because the run is marked terminal.
#[tokio::test]
async fn cancelling_while_an_upload_is_missing_ends_the_generation() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.tick().await;
    let bytes = b"cancelled before upload";
    let manifest = vec![ArtifactRef {
        source: "render".into(),
        sha256: domain::hash::sha256_hex(bytes),
        mime: "image/svg+xml".into(),
        size: bytes.len() as u64,
    }];
    h.pipeline_receipt(g.id, JobStatus::Succeeded, manifest).await.unwrap();
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Running);

    let out = h.op.cancel_generation(g.id).await.unwrap();
    assert_eq!(out.state, GenerationState::Cancelled, "{:?}", out.actions);
    h.tick().await;
    let cancelled = h.generation(item.id, g.id).await;
    assert_eq!(cancelled.state, GenerationState::Cancelled, "the deadline must not revive a cancelled request");
    assert_eq!(h.attempt(item.id, g.id).await.state, AttemptState::Cancelled);
    h.server.abort();
}

/// `ReceiptIn::artifacts` defaults to empty, so a succeeded receipt without a
/// manifest would otherwise complete an empty generation.
#[tokio::test]
async fn a_succeeded_receipt_without_a_manifest_is_rejected() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.tick().await;
    let err = h.pipeline_receipt(g.id, JobStatus::Succeeded, Vec::new()).await.unwrap_err();
    assert_eq!(code(err), "manifest_required");
    let after = h.generation(item.id, g.id).await;
    assert_ne!(after.state, GenerationState::Succeeded, "an empty generation is not a success");
    assert_ne!(h.attempt(item.id, g.id).await.state, AttemptState::Succeeded);
    h.server.abort();
}

/// A digest is normalized before storage, so the upload that names the same
/// bytes in lowercase links instead of silently matching no row.
#[tokio::test]
async fn an_uppercase_manifest_digest_is_normalized_and_the_upload_links() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.tick().await;
    let bytes = b"render bytes with an uppercase manifest digest";
    let lower = domain::hash::sha256_hex(bytes);
    let manifest =
        vec![ArtifactRef { source: "render".into(), sha256: lower.to_uppercase(), mime: "image/svg+xml".into(), size: bytes.len() as u64 }];
    assert_eq!(h.pipeline_receipt(g.id, JobStatus::Succeeded, manifest).await.unwrap().state, GenerationState::Running);
    let stored: String =
        domain::sqlx::query_scalar("select sha256 from generation_artifacts where generation_request_id = $1 and source = 'render'")
            .bind(g.id)
            .fetch_one(&h.hub.db)
            .await
            .unwrap();
    assert_eq!(stored, lower, "a digest is stored canonical");

    let resp = h.upload(g.id, "render", "image/svg+xml", &lower, bytes).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "{}", resp.text().await.unwrap_or_default());
    assert_eq!(h.unlinked(g.id).await, 0, "the upload links to its manifest row");
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Succeeded);
    h.server.abort();
}

/// A failed run ends the request immediately; the generation state is derived
/// from the attempt, not left at its pre-receipt value.
#[tokio::test]
async fn a_failed_receipt_fails_the_generation_immediately() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.tick().await;
    let failed = h.pipeline_receipt(g.id, JobStatus::Failed, Vec::new()).await.unwrap();
    assert_eq!(failed.state, GenerationState::Failed, "a failed run ends the request");
    assert_eq!(h.attempt(item.id, g.id).await.state, AttemptState::Failed);
    h.server.abort();
}

/// A deferral is a succeeded job that did no work. The run is not replaced:
/// the attempt fails with the reason, the generation fails, and the operator
/// re-requests.
#[tokio::test]
async fn a_deferred_run_fails_the_generation() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline(); // no uploads or receipt: the status read decides
    h.fx.windmill.as_ref().unwrap().defer_next_status.store(true, std::sync::atomic::Ordering::SeqCst);
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.tick().await; // submit, then the first status read reports the deferral

    let failed = h.generation(item.id, g.id).await;
    assert_eq!(failed.state, GenerationState::Failed, "a deferred run ends the request");
    let attempt = h.attempt(item.id, g.id).await;
    assert_eq!(attempt.state, AttemptState::Failed);
    assert!(
        attempt.progress["error"].as_str().unwrap_or_default().contains("deferred"),
        "the failure names the deferral: {:?}",
        attempt.progress
    );
    let raw = h.attempt_raw(g.id).await;
    assert_eq!(raw["deferred"], json!("GPU busy (fixture)"), "the reason is kept on the attempt");
    let deferrals: i64 =
        domain::sqlx::query_scalar("select count(*) from audit_log where action = 'generation.pipeline_deferred' and subject_id = $1")
            .bind(g.id.to_string())
            .fetch_one(&h.hub.db)
            .await
            .unwrap();
    assert_eq!(deferrals, 1, "the deferral is audited once");
    // Nothing resubmits: one attempt, one delivered outbox row.
    let attempts: i64 =
        domain::sqlx::query_scalar("select count(*) from execution_attempts where generation_request_id = $1 and kind = 'pipeline_run'")
            .bind(g.id)
            .fetch_one(&h.hub.db)
            .await
            .unwrap();
    assert_eq!(attempts, 1, "a deferral never queues a replacement run");
    let rows: i64 =
        domain::sqlx::query_scalar("select count(*) from outbox where topic = 'pipeline_run'").fetch_one(&h.hub.db).await.unwrap();
    assert_eq!(rows, 1, "no successor submission row exists");
    h.server.abort();
}

/// A claimed submission whose run handle never arrived must never be submitted
/// again: the process may have died after the backend accepted the first one.
#[tokio::test]
async fn a_claimed_submission_without_a_run_is_not_resubmitted() {
    let Some(h) = harness().await else { return };
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    // Simulate a crash between the committed claim and the recorded handle.
    domain::sqlx::query(
        "update execution_attempts set raw = raw || jsonb_build_object('submit_started_at', now())
          where generation_request_id = $1 and kind = 'pipeline_run'",
    )
    .bind(g.id)
    .execute(&h.hub.db)
    .await
    .unwrap();
    h.tick().await;

    let after = h.generation(item.id, g.id).await;
    assert_eq!(after.state, GenerationState::Failed, "an unknown submission ends the request");
    let attempt = h.attempt(item.id, g.id).await;
    assert_eq!(attempt.state, AttemptState::Failed);
    assert!(attempt.progress["error"].as_str().unwrap_or_default().contains("submission outcome unknown"), "{:?}", attempt.progress);
    let raw = h.attempt_raw(g.id).await;
    assert!(raw.get("run").is_none(), "no second run was adopted: {raw}");
    let (state, attempts): (String, i32) = domain::sqlx::query_as("select state, attempts from outbox where idempotency_key = $1")
        .bind(format!("generation:{}", g.id))
        .fetch_one(&h.hub.db)
        .await
        .unwrap();
    assert_eq!(state, "dead", "the submission row is dead-lettered, not retried");
    assert_eq!(attempts, 1, "one delivery observed the claim and stopped");
    h.server.abort();
}

/// The crash window is recoverable: the claim is committed, the handle is not,
/// and the backend still knows the run. The delivery adopts it and never
/// submits the work a second time.
#[tokio::test]
async fn a_claimed_submission_adopts_the_run_the_backend_knows() {
    let Some(h) = harness().await else { return };
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    // A crash between the committed claim and the record, with the run started.
    domain::sqlx::query(
        "update execution_attempts set raw = raw || jsonb_build_object('submit_started_at', now())
          where generation_request_id = $1 and kind = 'pipeline_run'",
    )
    .bind(g.id)
    .execute(&h.hub.db)
    .await
    .unwrap();
    h.fx.windmill.as_ref().unwrap().adopt_on_find.store(true, std::sync::atomic::Ordering::SeqCst);
    h.tick().await;

    let raw = h.attempt_raw(g.id).await;
    let adopted = raw["run"]["external_id"].as_str().unwrap_or_default();
    assert_eq!(adopted, format!("adopted-generation:{}:pipeline", g.id), "the found run is adopted: {raw}");
    assert!(matches!(h.generation(item.id, g.id).await.state, GenerationState::Dispatched | GenerationState::Running));
    assert_eq!(h.attempt(item.id, g.id).await.state, AttemptState::Queued, "the adopted run is followed");
    assert_eq!(h.fx.windmill.as_ref().unwrap().submits.load(std::sync::atomic::Ordering::SeqCst), 0, "never submitted again");
    h.server.abort();
}

/// A lookup that fails proves nothing either way: the claim and the run both
/// survive, and a later delivery adopts instead of resubmitting.
#[tokio::test]
async fn a_claim_lookup_outage_keeps_the_claim_and_adopts_later() {
    let Some(h) = harness().await else { return };
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    domain::sqlx::query(
        "update execution_attempts set raw = raw || jsonb_build_object('submit_started_at', now())
          where generation_request_id = $1 and kind = 'pipeline_run'",
    )
    .bind(g.id)
    .execute(&h.hub.db)
    .await
    .unwrap();
    let windmill = h.fx.windmill.as_ref().unwrap();
    // An outage longer than the whole delivery budget (12 attempts) must not
    // dead-letter the claim: lookups defer, they do not spend retries.
    let outages: usize = 15;
    windmill.find_outages.store(outages, std::sync::atomic::Ordering::SeqCst);
    windmill.adopt_on_find.store(true, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..outages {
        domain::sqlx::query("update outbox set next_attempt_at = now() where idempotency_key = $1")
            .bind(format!("generation:{}", g.id))
            .execute(&h.hub.db)
            .await
            .unwrap();
        h.tick().await;
    }

    let raw = h.attempt_raw(g.id).await;
    assert!(raw.get("run").is_none(), "an outage invents no handle: {raw}");
    assert!(raw.get("submit_started_at").is_some(), "the claim stays: {raw}");
    assert_ne!(h.attempt(item.id, g.id).await.state, AttemptState::Failed, "an outage is not a submission outcome");
    let (state, attempts): (String, i32) = domain::sqlx::query_as("select state, attempts from outbox where idempotency_key = $1")
        .bind(format!("generation:{}", g.id))
        .fetch_one(&h.hub.db)
        .await
        .unwrap();
    assert_eq!((state.as_str(), attempts), ("pending", 0), "a deferred lookup spends no delivery attempt");

    // The next delivery reaches the backend: the run is adopted, not resubmitted.
    domain::sqlx::query("update outbox set next_attempt_at = now() where idempotency_key = $1")
        .bind(format!("generation:{}", g.id))
        .execute(&h.hub.db)
        .await
        .unwrap();
    h.tick().await;
    let raw = h.attempt_raw(g.id).await;
    let adopted = format!("adopted-generation:{}:pipeline", g.id);
    assert_eq!(raw["run"]["external_id"].as_str(), Some(adopted.as_str()), "{raw}");
    assert_eq!(h.fx.windmill.as_ref().unwrap().submits.load(std::sync::atomic::Ordering::SeqCst), 0, "never submitted again");
    h.server.abort();
}

/// A worker downloads its stage inputs by asset id: the `procedure` role may
/// read asset bytes, and no other read widens.
#[tokio::test]
async fn a_procedure_may_download_asset_bytes_and_nothing_else() {
    let Some(h) = harness().await else { return };
    let item = h.brief(None).await;
    let g = h.produce(item.id, 1).await;
    assert_eq!(g.state, GenerationState::Succeeded);
    let asset = h.op.content_detail(item.id).await.unwrap().assets.remove(0);

    let (mime, bytes) = h.procedure.get_bytes(&format!("/assets/{}/content", asset.id)).await.unwrap();
    assert_eq!(mime, asset.mime);
    assert_eq!(domain::hash::sha256_hex(&bytes), asset.sha256, "the worker can verify what it downloaded");

    let denied = h.procedure.get::<Vec<ContentItem>>("/content").await.unwrap_err();
    assert!(matches!(denied, ClientError::Api { status: 403, .. }), "a procedure still cannot read content: {denied}");
    let denied = h.procedure.assets().await.unwrap_err();
    assert!(matches!(denied, ClientError::Api { status: 403, .. }), "asset listings are not widened either: {denied}");
    h.server.abort();
}

/// Cancelling while the backend has accepted a run but the hub has not
/// recorded its handle must stop that run, never adopt it as live.
#[tokio::test]
async fn cancelling_while_a_submission_is_in_flight_stops_the_run() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline(); // the fixture run stays pending until cancelled
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    let payload: serde_json::Value = domain::sqlx::query_scalar("select payload from outbox where idempotency_key = $1")
        .bind(format!("generation:{}", g.id))
        .fetch_one(&h.hub.db)
        .await
        .unwrap();
    let job: domain::production::PipelineJob = serde_json::from_value(payload).unwrap();

    let windmill = h.fx.windmill.as_ref().unwrap().clone();
    windmill.pause_submit.store(true, std::sync::atomic::Ordering::SeqCst);
    let started = windmill.submit_started.notified();
    let hub = h.hub.clone();
    let delivery = tokio::spawn(async move { domain::production::deliver_pipeline(&hub, &job).await });
    tokio::time::timeout(Duration::from_secs(5), started).await.expect("the submission reached the backend");

    let out = h.op.cancel_generation(g.id).await.unwrap();
    assert_eq!(out.state, GenerationState::Cancelled);
    windmill.submit_release.notify_one();
    delivery.await.unwrap().unwrap();

    let attempt = h.attempt(item.id, g.id).await;
    assert_eq!(attempt.state, AttemptState::Cancelled);
    let raw = h.attempt_raw(g.id).await;
    assert!(raw.get("run").is_none(), "a cancelled attempt never adopts the run as live: {raw}");
    let run_id = raw["cancelled_run"]["external_id"].as_str().expect("the stopped handle is recorded");
    let run = domain::ports::PipelineRun { external_id: run_id.into(), raw: json!({}) };
    let status = h.hub.pipelines.get("fixture.svg").unwrap().status(&run).await.unwrap();
    assert_eq!(status, domain::ports::PipelineStatus::Cancelled, "the backend run was stopped");
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Cancelled);
    h.server.abort();
}

/// The terminal manifest is the generation's exact source set: a pre-upload it
/// omits and a replay that is a subset or superset are all conflicts.
#[tokio::test]
async fn a_manifest_must_match_the_uploaded_source_set() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;

    // A pre-upload the manifest does not name contradicts the run.
    let g = h.pipeline(item.id).await;
    h.tick().await;
    let extra = b"an output the manifest forgot";
    let resp = h.upload(g.id, "extra", "text/plain", &domain::hash::sha256_hex(extra), extra).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK, "{}", resp.text().await.unwrap_or_default());
    let render = b"the only recorded render";
    let manifest = vec![ArtifactRef {
        source: "render".into(),
        sha256: domain::hash::sha256_hex(render),
        mime: "image/svg+xml".into(),
        size: render.len() as u64,
    }];
    let err = h.pipeline_receipt(g.id, JobStatus::Succeeded, manifest).await.unwrap_err();
    assert_eq!(code(err), "manifest_conflict");
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Failed, "the contradiction ends the run");

    // A recorded manifest only accepts an exact replay.
    let g2 = h.pipeline(item.id).await;
    h.tick().await;
    let caption = b"a caption";
    let full = vec![
        ArtifactRef {
            source: "render".into(),
            sha256: domain::hash::sha256_hex(render),
            mime: "image/svg+xml".into(),
            size: render.len() as u64,
        },
        ArtifactRef {
            source: "caption".into(),
            sha256: domain::hash::sha256_hex(caption),
            mime: "text/plain".into(),
            size: caption.len() as u64,
        },
    ];
    assert_eq!(h.pipeline_receipt(g2.id, JobStatus::Succeeded, full.clone()).await.unwrap().state, GenerationState::Running);
    let err = h.pipeline_receipt(g2.id, JobStatus::Succeeded, vec![full[0].clone()]).await.unwrap_err();
    assert_eq!(code(err), "manifest_conflict", "a subset replay is a conflict");
    let mut superset = full.clone();
    superset.push(ArtifactRef { source: "late".into(), sha256: domain::hash::sha256_hex(b"late"), mime: "text/plain".into(), size: 4 });
    let err = h.pipeline_receipt(g2.id, JobStatus::Succeeded, superset).await.unwrap_err();
    assert_eq!(code(err), "manifest_conflict", "a superset replay is a conflict");
    assert_eq!(h.generation(item.id, g2.id).await.state, GenerationState::Running, "the recorded manifest stands");
    h.server.abort();
}

/// Wait until one query is blocked on the `generation_requests` row, so a test
/// can interleave a cancel and a refresh deterministically.
async fn wait_for_generation_row_wait(hub: &domain::Hub) {
    for _ in 0..400 {
        let blocked: i64 = domain::sqlx::query_scalar(
            "select count(*) from pg_stat_activity
              where state = 'active' and wait_event_type = 'Lock' and query like '%generation_requests%'",
        )
        .fetch_one(&hub.db)
        .await
        .unwrap();
        if blocked > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the refresh never waited for the generation row lock");
}

/// A refresh that read an open attempt must not revive a request a cancel just
/// ended: the attempts and the current state are read under the generation row
/// lock, and a terminal generation is final.
#[tokio::test]
async fn a_stale_refresh_cannot_revive_a_cancelled_generation() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    // Deliver the submission only: the request is `dispatched` with a queued
    // attempt, before any status read moved it to `running`.
    let payload: serde_json::Value = domain::sqlx::query_scalar("select payload from outbox where idempotency_key = $1")
        .bind(format!("generation:{}", g.id))
        .fetch_one(&h.hub.db)
        .await
        .unwrap();
    let job: domain::production::PipelineJob = serde_json::from_value(payload).unwrap();
    assert_eq!(domain::production::deliver_pipeline(&h.hub, &job).await.unwrap(), domain::production::PipelineDelivery::Done);
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Dispatched);
    assert_eq!(h.attempt(item.id, g.id).await.state, AttemptState::Queued);

    // The cancel writes its attempt and request while the request row is still
    // locked; a refresh that read the attempt before this point derives
    // `running` and (pre-fix) overwrote the cancelled request.
    let mut cancel = h.hub.db.begin().await.unwrap();
    domain::sqlx::query("update generation_requests set state = 'cancelled' where id = $1").bind(g.id).execute(&mut *cancel).await.unwrap();
    domain::sqlx::query("update execution_attempts set state = 'cancelled' where generation_request_id = $1 and kind = 'pipeline_run'")
        .bind(g.id)
        .execute(&mut *cancel)
        .await
        .unwrap();
    let hub = h.hub.clone();
    let refresh = tokio::spawn(async move { domain::production::refresh_generation_state(&hub, g.id).await });
    tokio::time::timeout(Duration::from_secs(10), wait_for_generation_row_wait(&h.hub))
        .await
        .expect("the refresh reaches the generation lock");
    cancel.commit().await.unwrap();
    refresh.await.unwrap().unwrap();

    assert_eq!(
        h.generation(item.id, g.id).await.state,
        GenerationState::Cancelled,
        "a refresh must not derive a state from attempts read before the cancel committed"
    );
    assert_eq!(h.attempt(item.id, g.id).await.state, AttemptState::Cancelled);
    h.server.abort();
}

/// The attempt write and the derived request state share a transaction, but a
/// crash between any two writes is still possible: the reconcile pass re-derives
/// every pipeline generation whose attempts all ended.
#[tokio::test]
async fn a_terminal_pipeline_attempt_without_a_refresh_is_recovered_by_reconcile() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.tick().await; // submit and one status read: the attempt is queued
    assert_eq!(h.attempt(item.id, g.id).await.state, AttemptState::Queued);
    // The crash window: a terminal attempt committed, its request never re-derived.
    domain::sqlx::query("update execution_attempts set state = 'failed' where generation_request_id = $1 and kind = 'pipeline_run'")
        .bind(g.id)
        .execute(&h.hub.db)
        .await
        .unwrap();
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Running, "the request is stranded: its attempt already ended");

    h.tick().await; // the recovery sweep
    assert_eq!(
        h.generation(item.id, g.id).await.state,
        GenerationState::Failed,
        "reconciliation derives the state the missed refresh left behind"
    );
    h.server.abort();
}

/// A submission row that exhausts its retries can never be delivered again: its
/// attempt must end, or the generation waits for a run nothing will follow.
#[tokio::test]
async fn a_pipeline_submission_that_exhausts_its_retries_fails_the_generation() {
    let Some(h) = harness().await else { return };
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    // Every delivery fails as unreachable: the claim is released, no run starts.
    h.fx.windmill.as_ref().unwrap().fail_submit.store(12, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..12 {
        h.tick().await;
        // Skip the outbox backoff; the delivery itself still fails.
        domain::sqlx::query("update outbox set next_attempt_at = now() where idempotency_key = $1")
            .bind(format!("generation:{}", g.id))
            .execute(&h.hub.db)
            .await
            .unwrap();
    }
    let (state, attempts): (String, i32) = domain::sqlx::query_as("select state, attempts from outbox where idempotency_key = $1")
        .bind(format!("generation:{}", g.id))
        .fetch_one(&h.hub.db)
        .await
        .unwrap();
    assert_eq!((state.as_str(), attempts), ("dead", 12), "twelve deliveries spend every retry");
    let after = h.generation(item.id, g.id).await;
    assert_eq!(after.state, GenerationState::Failed, "a submission that can never be delivered ends its request");
    let attempt = h.attempt(item.id, g.id).await;
    assert_eq!(attempt.state, AttemptState::Failed);
    assert!(attempt.progress["error"].as_str().unwrap_or_default().contains("fixture pipeline is down"), "{:?}", attempt.progress);
    assert!(h.attempt_raw(g.id).await.get("run").is_none(), "nothing was submitted");
    h.server.abort();
}

/// A run the backend started but the hub could not record is dead-lettered. The
/// attempt must end with its submission claim kept, so the work is never
/// submitted a second time.
#[tokio::test]
async fn a_pipeline_run_started_but_not_recorded_is_failed_when_the_row_dies() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    // The record fails after the backend accepted the run: no write of a run
    // handle reaches the attempt, exactly like a lost record.
    domain::sqlx::query(
        "create function deny_run_handle() returns trigger language plpgsql as $$
         begin
           if new.raw ? 'run' then raise exception 'recording the run handle failed (test)'; end if;
           return new;
         end $$",
    )
    .execute(&h.hub.db)
    .await
    .unwrap();
    domain::sqlx::query(
        "create trigger deny_run_handle before update on execution_attempts for each row execute function deny_run_handle()",
    )
    .execute(&h.hub.db)
    .await
    .unwrap();
    h.tick().await;

    let after = h.generation(item.id, g.id).await;
    assert_eq!(after.state, GenerationState::Failed, "a started run the hub cannot record ends the request");
    let attempt = h.attempt(item.id, g.id).await;
    assert_eq!(attempt.state, AttemptState::Failed);
    assert!(attempt.progress["error"].as_str().unwrap_or_default().contains("started but not recorded"), "{:?}", attempt.progress);
    let raw = h.attempt_raw(g.id).await;
    assert!(raw.get("run").is_none(), "the handle was never adopted: {raw}");
    assert!(raw.get("submit_started_at").is_some(), "the submission claim stays, so nothing resubmits: {raw}");
    let (state, attempts): (String, i32) = domain::sqlx::query_as("select state, attempts from outbox where idempotency_key = $1")
        .bind(format!("generation:{}", g.id))
        .fetch_one(&h.hub.db)
        .await
        .unwrap();
    assert_eq!((state.as_str(), attempts), ("dead", 1), "the row is dead, not retried");
    h.server.abort();
}

/// A finished run whose manifest the hub cannot accept is a failed run, not a
/// transient read: it is ended through the run fence instead of being retried
/// on every tick.
#[tokio::test]
async fn a_completed_run_with_an_unusable_manifest_fails_the_generation() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline(); // no receipt or uploads: the status read decides
    let item = h.brief(None).await;
    let g = h.pipeline(item.id).await;
    h.fx.windmill.as_ref().unwrap().bad_manifest.store(true, std::sync::atomic::Ordering::SeqCst);
    h.tick().await;

    let after = h.generation(item.id, g.id).await;
    assert_eq!(after.state, GenerationState::Failed, "an invalid terminal manifest is a failed run");
    let attempt = h.attempt(item.id, g.id).await;
    assert_eq!(attempt.state, AttemptState::Failed);
    let error = attempt.progress["error"].as_str().unwrap_or_default();
    assert!(error.contains("sha256"), "the failure names the invalid digest: {error}");
    // Nothing is left to retry: the run is out of the open set, never resubmitted.
    h.tick().await;
    assert_eq!(h.generation(item.id, g.id).await.state, GenerationState::Failed);
    let attempts: i64 =
        domain::sqlx::query_scalar("select count(*) from execution_attempts where generation_request_id = $1 and kind = 'pipeline_run'")
            .bind(g.id)
            .fetch_one(&h.hub.db)
            .await
            .unwrap();
    assert_eq!(attempts, 1, "the failed run is never replaced");
    h.server.abort();
}

/// A status read that never answers must not hold up the local repair passes:
/// they run first, and each remote read is bounded on its own.
#[tokio::test]
async fn a_slow_status_read_does_not_delay_the_upload_deadline() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    // The run whose status read will park.
    let open = h.pipeline(item.id).await;
    // The run whose uploads never arrive.
    let overdue = h.pipeline(item.id).await;
    h.tick().await;
    let bytes = b"an upload that never arrives";
    let manifest = vec![ArtifactRef {
        source: "render".into(),
        sha256: domain::hash::sha256_hex(bytes),
        mime: "image/svg+xml".into(),
        size: bytes.len() as u64,
    }];
    h.pipeline_receipt(overdue.id, JobStatus::Succeeded, manifest).await.unwrap();
    domain::sqlx::query(
        "update execution_attempts set raw = raw || '{\"manifest_at\": 0}'::jsonb
          where generation_request_id = $1 and kind = 'pipeline_run'",
    )
    .bind(overdue.id)
    .execute(&h.hub.db)
    .await
    .unwrap();

    let windmill = h.fx.windmill.as_ref().unwrap().clone();
    windmill.slow_status.store(true, std::sync::atomic::Ordering::SeqCst);
    let started = windmill.status_started.notified();
    let hub = h.hub.clone();
    let pass = tokio::spawn(async move { worker::reconcile(&hub).await });
    tokio::time::timeout(Duration::from_secs(10), started).await.expect("the status read reached the slow backend");
    // The deadline has already fired, while the status read is still parked.
    assert_eq!(
        h.generation(item.id, overdue.id).await.state,
        GenerationState::Failed,
        "the deadline pass runs before the remote status reads"
    );
    // The parked read is bounded on its own: the pass ends instead of waiting
    // for a backend that never answers.
    tokio::time::timeout(Duration::from_secs(15), pass).await.expect("one slow status read does not hold the pass").unwrap().unwrap();
    assert_ne!(h.generation(item.id, open.id).await.state, GenerationState::Failed);
    h.server.abort();
}

/// A channel whose reply read-back reports a definite provider failure.
struct FailedReplyLookup;

#[async_trait::async_trait]
impl domain::ports::Publisher for FailedReplyLookup {
    async fn submit(&self, _: &domain::ports::PublishRequest) -> domain::ports::PortResult<domain::ports::ProviderPost> {
        panic!("no publication is expected in this test");
    }
    async fn lookup(&self, _: &str, _: &str) -> domain::ports::PortResult<Option<domain::ports::ProviderPost>> {
        Ok(None)
    }
    async fn reply(&self, _: &domain::ports::ReplyRequest) -> domain::ports::PortResult<domain::ports::ProviderPost> {
        panic!("an unknown reply must never be resent");
    }
    async fn lookup_reply(&self, _: &str, _: &str) -> domain::ports::PortResult<Option<domain::ports::ProviderPost>> {
        Ok(Some(domain::ports::ProviderPost {
            provider_id: String::new(),
            state: domain::ports::ProviderState::Failed,
            url: None,
            raw: json!({ "state": "failed" }),
        }))
    }
}

#[tokio::test]
async fn a_reply_the_provider_reports_failed_reconciles_as_rejected_not_sent() {
    let Some(h) = harness().await else { return };
    h.add_account("test-failed-reply", "plinth").await;
    let ev = InteractionIn {
        account_id: "test-failed-reply".into(),
        external_thread_id: "t-failed".into(),
        external_id: "m-failed".into(),
        direction: Direction::Inbound,
        author: "@fan".into(),
        body: "Still there?".into(),
        occurred_at: Utc::now(),
        permalink: None,
        raw: json!({}),
    };
    let r: IngestReceipt = h.ingest.post("/ingest/interactions", &ev).await.unwrap();
    let draft = h.agent.draft_reply(r.id, &DraftReplyIn { in_reply_to: None, body: "Yes!".into(), owner_epoch: 1 }).await.unwrap();
    h.op.decide_draft(draft.id, &DraftDecisionIn::Approve).await.unwrap();
    // The response was lost: the hub only knows the outcome is unknown.
    domain::sqlx::query("update reply_drafts set state = 'outcome_unknown' where id = $1").bind(draft.id).execute(&h.hub.db).await.unwrap();
    let mut channels = (*h.hub.channels).clone();
    channels.insert("plinth".into(), Arc::new(FailedReplyLookup) as Arc<dyn domain::ports::Publisher>);
    let hub = domain::Hub { channels: Arc::new(channels), ..h.hub.clone() };
    domain::engagement::reconcile_replies(&hub).await.unwrap();
    let state: String =
        domain::sqlx::query_scalar("select state from reply_drafts where id = $1").bind(draft.id).fetch_one(&h.hub.db).await.unwrap();
    assert_eq!(state, "rejected", "a failed provider record is not a sent reply");
}

#[tokio::test]
async fn open_procedures_are_polled_least_recently_first_so_none_starves() {
    let Some(h) = harness().await else { return };
    h.manual_pipeline();
    let item = h.brief(None).await;
    let generation = h.pipeline(item.id).await;
    // More open root jobs than one poll batch (20) holds.
    for i in 0..25 {
        domain::sqlx::query(
            "insert into execution_attempts (id, generation_request_id, kind, external_id, state)
             values ($1, $2, 'windmill_root', $3, 'running')",
        )
        .bind(Uuid::new_v4())
        .bind(generation.id)
        .bind(format!("stalled-root-{i}"))
        .execute(&h.hub.db)
        .await
        .unwrap();
    }
    let unpolled = || async {
        domain::sqlx::query_scalar::<_, i64>(
            "select count(*) from execution_attempts where kind = 'windmill_root' and external_id like 'stalled-root-%' and not raw ? 'polled_at'",
        )
        .fetch_one(&h.hub.db)
        .await
        .unwrap()
    };
    domain::production::reconcile_procedures(&h.hub).await.unwrap();
    assert_eq!(unpolled().await, 5, "one pass polls one batch");
    domain::production::reconcile_procedures(&h.hub).await.unwrap();
    assert_eq!(unpolled().await, 0, "the next pass takes the ones never polled before any repeat");
}

// ── Opportunities (stream H1) ───────────────────────────────────────────────

fn opportunity_in(external_id: &str, deadline: Option<chrono::DateTime<Utc>>) -> OpportunityIn {
    OpportunityIn {
        persona_id: None,
        source: "n8n:funder-feed".into(),
        external_id: external_id.into(),
        kind: OpportunityKind::Grant,
        title: format!("Call {external_id}"),
        funder: "Example Fund".into(),
        url: format!("https://fund.example/{external_id}"),
        deadline,
        amount: Some("10k".into()),
        eligibility: "early career".into(),
        fit: "spike sorting".into(),
        evidence: vec![Evidence { url: format!("https://fund.example/{external_id}"), quote: "a call".into() }],
    }
}

async fn decide(h: &H, id: Uuid, state: OpportunityState) -> Opportunity {
    h.op.decide_opportunity(id, &OpportunityDecisionIn { state, note: None, paperclip_issue_id: None }).await.unwrap()
}

#[tokio::test]
async fn opportunity_ingest_replay_keeps_state_and_updates_facts() {
    let Some(h) = harness().await else { return };
    let first = h.ingest.ingest_opportunity(&opportunity_in("call-1", Some(Utc::now() + chrono::Duration::days(3)))).await.unwrap();
    h.op.decide_opportunity(
        first.id,
        &OpportunityDecisionIn { state: OpportunityState::Shortlisted, note: Some("apply".into()), paperclip_issue_id: None },
    )
    .await
    .unwrap();

    let mut replay = opportunity_in("call-1", Some(Utc::now() + chrono::Duration::days(9)));
    replay.title = "Call 1, extended".into();
    replay.amount = Some("20k".into());
    replay.fit = "a better fit".into();
    let again = h.ingest.ingest_opportunity(&replay).await.unwrap();
    assert_eq!(again.id, first.id, "a replay converges on one row");
    assert_eq!(again.title, "Call 1, extended");
    assert_eq!(again.amount.as_deref(), Some("20k"));
    assert_eq!(again.fit, "a better fit");
    assert_eq!(again.state, OpportunityState::Shortlisted, "ingest never overwrites the decision");
    assert_eq!(again.note.as_deref(), Some("apply"));
    assert_eq!(h.op.opportunities(None, None).await.unwrap().len(), 1);
    let history = h.op.get::<Vec<HistoryEntry>>(&format!("/history/opportunity/{}", first.id)).await.unwrap();
    assert!(history.iter().any(|entry| entry.action == "opportunity.decided"));
    assert!(history.iter().any(|entry| entry.action == "opportunity.reingested"));
}

#[tokio::test]
async fn opportunity_expired_is_derived_while_the_call_is_open() {
    let Some(h) = harness().await else { return };
    let past = h.ingest.ingest_opportunity(&opportunity_in("past", Some(Utc::now() - chrono::Duration::days(1)))).await.unwrap();
    assert!(past.expired);
    assert!(decide(&h, past.id, OpportunityState::Drafting).await.expired, "drafting is still open");
    assert!(!decide(&h, past.id, OpportunityState::Submitted).await.expired, "a submitted call is never expired");
    let rolling = h.ingest.ingest_opportunity(&opportunity_in("rolling", None)).await.unwrap();
    assert!(!rolling.expired, "a null deadline is rolling, not expired");
}

#[tokio::test]
async fn an_agent_may_file_opportunities_but_never_decide_them() {
    let Some(h) = harness().await else { return };
    let filed = h.agent.ingest_opportunity(&opportunity_in("agent-call", None)).await.unwrap();
    let refused = h
        .agent
        .decide_opportunity(filed.id, &OpportunityDecisionIn { state: OpportunityState::Shortlisted, note: None, paperclip_issue_id: None })
        .await
        .unwrap_err();
    assert_eq!(code(refused), "forbidden");
    assert_eq!(h.op.opportunity(filed.id).await.unwrap().state, OpportunityState::New);
}

#[tokio::test]
async fn today_lists_open_calls_due_within_two_weeks() {
    let Some(h) = harness().await else { return };
    let soon = h.ingest.ingest_opportunity(&opportunity_in("soon", Some(Utc::now() + chrono::Duration::days(3)))).await.unwrap();
    let soon = decide(&h, soon.id, OpportunityState::Shortlisted).await;
    let drafting = h.ingest.ingest_opportunity(&opportunity_in("drafting", Some(Utc::now() + chrono::Duration::days(10)))).await.unwrap();
    let drafting = decide(&h, drafting.id, OpportunityState::Drafting).await;
    let later = h.ingest.ingest_opportunity(&opportunity_in("later", Some(Utc::now() + chrono::Duration::days(30)))).await.unwrap();
    let later = decide(&h, later.id, OpportunityState::Shortlisted).await;
    let undecided = h.ingest.ingest_opportunity(&opportunity_in("undecided", Some(Utc::now() + chrono::Duration::days(2)))).await.unwrap();
    let missed = h.ingest.ingest_opportunity(&opportunity_in("missed", Some(Utc::now() - chrono::Duration::days(1)))).await.unwrap();
    let missed = decide(&h, missed.id, OpportunityState::Drafting).await;

    let today = h.op.today().await.unwrap();
    let ids: Vec<Uuid> = today.opportunities.iter().map(|item| item.subject_id).collect();
    assert!(ids.contains(&soon.id), "shortlisted and due in three days");
    assert!(ids.contains(&drafting.id), "drafting and due in ten days");
    let status = |id: Uuid| today.opportunities.iter().find(|item| item.subject_id == id).map(|item| item.status.clone());
    assert_eq!(status(missed.id).as_deref(), Some("expired"), "a passed deadline shows as expired");
    assert_eq!(status(drafting.id).as_deref(), Some("drafting"));
    assert_eq!(h.op.opportunity(missed.id).await.unwrap().state, OpportunityState::Drafting, "the stored state is kept");
    assert!(!ids.contains(&later.id), "thirty days is outside the window");
    assert!(!ids.contains(&undecided.id), "a new call is not on the decision queue");
    assert!(today.opportunities.iter().all(|item| item.subject_type == "opportunity"));

    let due = h.op.opportunities(None, Some(7)).await.unwrap();
    let ids: Vec<Uuid> = due.iter().map(|item| item.id).collect();
    assert!(ids.contains(&soon.id) && ids.contains(&undecided.id));
    assert!(!ids.contains(&drafting.id) && !ids.contains(&later.id));
}

// ── Renewals (stream H3) ────────────────────────────────────────────────────

/// Apply only renewals through the real seed path, as a restart would.
async fn seed_renewals(h: &H, renewals: serde_json::Value) {
    let seed: bootstrap::Seed = serde_json::from_value(json!({ "renewals": renewals })).unwrap();
    bootstrap::apply_seed(&h.hub, &seed, "fixture-company").await.unwrap();
}

fn renewal_seed(key: &str, cadence_days: Option<i32>, due_at: Option<chrono::DateTime<Utc>>) -> serde_json::Value {
    json!({
        "key": key,
        "title": format!("Token {key}"),
        "procedure": "Re-authorize and update the credential.",
        "cadence_days": cadence_days,
        "due_at": due_at,
    })
}

/// PostgreSQL keeps microseconds and the request takes a moment, so compare
/// clock assertions to the second.
fn within(actual: chrono::DateTime<Utc>, expected: chrono::DateTime<Utc>) -> bool {
    (actual - expected).num_seconds().abs() <= 2
}

async fn renewal(h: &H, key: &str) -> Renewal {
    h.op.renewals().await.unwrap().into_iter().find(|row| row.key == key).unwrap()
}

#[tokio::test]
async fn re_seeding_refreshes_facts_but_never_moves_the_clock() {
    let Some(h) = harness().await else { return };
    let seeded_due = Utc::now() + chrono::Duration::days(5);
    let mut seeded = renewal_seed("seed-a", Some(60), Some(seeded_due));
    seeded["urls"] = json!(["https://mastodon.social/oauth/authorized_applications"]);
    seed_renewals(&h, json!([seeded])).await;
    let row = renewal(&h, "seed-a").await;
    assert_eq!(row.cadence_days, Some(60));
    assert_eq!(row.warn_days, 14, "the seed default");
    assert_eq!(row.urls, ["https://mastodon.social/oauth/authorized_applications"], "guide URLs round-trip");
    assert!(within(row.due_at.unwrap(), seeded_due));
    assert!(row.renewed_at.is_none());

    // The operator renews, then a restart re-seeds the same key.
    let renewed_due = Utc::now() + chrono::Duration::days(9);
    let renewed = h.op.renewal_renewed("seed-a", &RenewalRenewedIn { due_at: Some(renewed_due) }).await.unwrap();
    assert!(renewed.renewed_at.is_some());
    let history = h.op.get::<Vec<HistoryEntry>>(&format!("/history/renewal/{}", renewed.id)).await.unwrap();
    assert!(history.iter().any(|entry| entry.action == "renewal.renewed"));

    let mut again = renewal_seed("seed-a", Some(30), Some(Utc::now() + chrono::Duration::days(90)));
    again["title"] = json!("Token seed-a, rotated");
    again["procedure"] = json!("Rotate in the provider console.");
    again["warn_days"] = json!(21);
    again["urls"] = json!(["https://example.org/tokens", "https://example.org/docs"]);
    seed_renewals(&h, json!([again])).await;
    let after = renewal(&h, "seed-a").await;
    assert_eq!(after.title, "Token seed-a, rotated");
    assert_eq!(after.procedure, "Rotate in the provider console.");
    assert_eq!(after.urls, ["https://example.org/tokens", "https://example.org/docs"], "a re-seed refreshes the URLs");
    assert_eq!(after.cadence_days, Some(30));
    assert_eq!(after.warn_days, 21);
    assert!(within(after.due_at.unwrap(), renewed_due), "the seed never moves a renewed due date");
    assert_eq!(after.renewed_at, renewed.renewed_at, "the seed never clears renewed_at");

    // A key seen for the first time takes the seed's due date.
    let fresh_due = Utc::now() + chrono::Duration::days(4);
    seed_renewals(&h, json!([renewal_seed("seed-b", Some(7), Some(fresh_due))])).await;
    let fresh = renewal(&h, "seed-b").await;
    assert!(within(fresh.due_at.unwrap(), fresh_due));
    assert!(fresh.renewed_at.is_none());
}

#[tokio::test]
async fn a_seeded_renewal_url_must_be_https() {
    let Some(h) = harness().await else { return };
    let mut insecure = renewal_seed("insecure", Some(30), None);
    insecure["urls"] = json!(["http://example.org/token"]);
    let seed: bootstrap::Seed = serde_json::from_value(json!({ "renewals": [insecure] })).unwrap();

    let err = format!("{:#}", bootstrap::apply_seed(&h.hub, &seed, "fixture-company").await.unwrap_err());
    assert!(err.contains("insecure") && err.contains("https://"), "the error names the key and the rule: {err}");
    assert!(h.op.renewals().await.unwrap().iter().all(|row| row.key != "insecure"), "a refused seed writes nothing");
}

#[tokio::test]
async fn renewing_advances_by_cadence_unless_an_explicit_due_wins() {
    let Some(h) = harness().await else { return };
    seed_renewals(&h, json!([renewal_seed("cadence", Some(60), None)])).await;
    assert!(renewal(&h, "cadence").await.due_at.is_none(), "unknown until the first renewal");

    let renewed = h.op.renewal_renewed("cadence", &RenewalRenewedIn { due_at: None }).await.unwrap();
    assert!(renewed.renewed_at.is_some());
    assert!(within(renewed.due_at.unwrap(), Utc::now() + chrono::Duration::days(60)), "the cadence starts the clock");

    let explicit = Utc::now() + chrono::Duration::days(3);
    let again = h.op.renewal_renewed("cadence", &RenewalRenewedIn { due_at: Some(explicit) }).await.unwrap();
    assert!(within(again.due_at.unwrap(), explicit), "an explicit due date wins over the cadence");
}

#[tokio::test]
async fn a_renewal_without_cadence_needs_an_explicit_due_date() {
    let Some(h) = harness().await else { return };
    seed_renewals(&h, json!([renewal_seed("no-cadence", None, None)])).await;

    let refused = h.op.renewal_renewed("no-cadence", &RenewalRenewedIn { due_at: None }).await.unwrap_err();
    assert_eq!(code(refused), "due_required");
    let row = renewal(&h, "no-cadence").await;
    assert!(row.due_at.is_none() && row.renewed_at.is_none(), "a refused renewal changes nothing");

    let due = Utc::now() + chrono::Duration::days(11);
    let renewed = h.op.renewal_renewed("no-cadence", &RenewalRenewedIn { due_at: Some(due) }).await.unwrap();
    assert!(within(renewed.due_at.unwrap(), due));

    let missing = h.op.renewal_renewed("nope", &RenewalRenewedIn { due_at: None }).await.unwrap_err();
    assert_eq!(code(missing), "not_found");
}

#[tokio::test]
async fn agents_can_read_renewals_but_never_mark_them() {
    let Some(h) = harness().await else { return };
    seed_renewals(&h, json!([renewal_seed("operator-only", Some(30), None)])).await;
    assert!(h.agent.renewals().await.unwrap().iter().any(|row| row.key == "operator-only"));
    let refused = h.agent.renewal_renewed("operator-only", &RenewalRenewedIn { due_at: None }).await.unwrap_err();
    assert_eq!(code(refused), "forbidden");
    assert!(renewal(&h, "operator-only").await.renewed_at.is_none());
}

#[tokio::test]
async fn today_lists_unknown_overdue_and_near_renewals_only() {
    let Some(h) = harness().await else { return };
    seed_renewals(
        &h,
        json!([
            renewal_seed("overdue", Some(60), Some(Utc::now() - chrono::Duration::days(2))),
            renewal_seed("unknown", Some(60), None),
            renewal_seed("within", Some(60), Some(Utc::now() + chrono::Duration::days(3))),
            renewal_seed("beyond", Some(60), Some(Utc::now() + chrono::Duration::days(30))),
        ]),
    )
    .await;

    let rows = h.op.renewals().await.unwrap();
    let id = |key: &str| rows.iter().find(|row| row.key == key).unwrap().id;
    let today = h.op.today().await.unwrap();
    let ids: Vec<Uuid> = today.renewals.iter().map(|item| item.subject_id).collect();
    assert!(ids.contains(&id("overdue")) && ids.contains(&id("unknown")) && ids.contains(&id("within")));
    assert!(!ids.contains(&id("beyond")), "thirty days is outside the warn window");
    let status = |key: &str| today.renewals.iter().find(|item| item.subject_id == id(key)).map(|item| item.status.clone());
    assert_eq!(status("overdue").as_deref(), Some("overdue"));
    assert_eq!(status("unknown").as_deref(), Some("unknown"));
    assert_eq!(status("within").as_deref(), Some("due"));
    let item = today.renewals.iter().find(|item| item.subject_id == id("within")).unwrap();
    assert_eq!(item.kind, "renewal");
    assert_eq!(item.subject_type, "renewal");
    assert!(within(item.at.unwrap(), rows.iter().find(|row| row.key == "within").unwrap().due_at.unwrap()));
}

#[tokio::test]
async fn provider_observations_judge_expiry_without_moving_the_renewal_clock() {
    let Some(h) = harness().await else { return };
    let mut provider = renewal_seed("provider-token", None, None);
    provider["expiry_kind"] = json!("provider");
    let mut never = renewal_seed("never-token", None, None);
    never["expiry_kind"] = json!("never");
    let cadence_due = Utc::now() + chrono::Duration::days(3);
    let cadence = renewal_seed("cadence-token", Some(60), Some(cadence_due));
    seed_renewals(&h, json!([provider, never, cadence])).await;

    // An unobserved provider renewal is unverifiable, never healthy and never
    // promoted to `never`; a `never` kind is its own state.
    let row = renewal(&h, "provider-token").await;
    assert_eq!(row.expiry_kind, ExpiryKind::Provider);
    assert_eq!(row.status, "unverifiable");
    assert!(row.observation.is_none() && row.observed_at.is_none() && row.last_good_at.is_none());
    let never_row = renewal(&h, "never-token").await;
    assert_eq!(never_row.expiry_kind, ExpiryKind::Never);
    assert_eq!(never_row.status, "never");
    assert_eq!(renewal(&h, "cadence-token").await.status, "due");

    // A procedure collector records a date. The operator clock does not move.
    let expires = Utc::now() + chrono::Duration::days(2);
    let observed = h
        .procedure
        .observe_renewal(
            "provider-token",
            &ObserveRenewalIn { outcome: Observation::Date, expires_at: Some(expires), observed_at: Utc::now() },
        )
        .await
        .unwrap();
    assert_eq!(observed.observation, Some(Observation::Date));
    assert!(observed.observed_at.is_some() && observed.last_good_at.is_some());
    assert!(within(observed.last_good_expires_at.unwrap(), expires));
    assert_eq!(observed.status, "due", "two days is inside the 14-day warn window");
    assert!(observed.due_at.is_none() && observed.renewed_at.is_none(), "an observation is not a rotation");

    // Today carries the judgement and never the `never` row.
    let today = h.op.today().await.unwrap();
    let provider_id = observed.id;
    let item = today.renewals.iter().find(|item| item.subject_id == provider_id).unwrap();
    assert_eq!(item.status, "due");
    assert!(within(item.at.unwrap(), expires), "the reported date is the deadline");
    assert!(!today.renewals.iter().any(|item| item.subject_id == never_row.id), "a never-expiring renewal is not a to-do");

    // A provider-confirmed inactive credential is invalid, and keeps the last
    // good deadline only as context.
    let inactive = h
        .procedure
        .observe_renewal("provider-token", &ObserveRenewalIn { outcome: Observation::Inactive, expires_at: None, observed_at: Utc::now() })
        .await
        .unwrap();
    assert_eq!(inactive.status, "invalid");
    assert_eq!(inactive.observation, Some(Observation::Inactive));
    assert_eq!(inactive.last_good_expires_at, observed.last_good_expires_at);
    let today = h.op.today().await.unwrap();
    assert_eq!(today.renewals.iter().find(|item| item.subject_id == provider_id).unwrap().status, "invalid");

    // Refusals: a future clock, a missing or unexpected `expires_at`, and a
    // renewal whose kind no collector may observe.
    let future = h
        .procedure
        .observe_renewal(
            "provider-token",
            &ObserveRenewalIn { outcome: Observation::Never, expires_at: None, observed_at: Utc::now() + chrono::Duration::seconds(61) },
        )
        .await
        .unwrap_err();
    assert_eq!(code(future), "observed_in_future");
    let missing = h
        .procedure
        .observe_renewal("provider-token", &ObserveRenewalIn { outcome: Observation::Date, expires_at: None, observed_at: Utc::now() })
        .await
        .unwrap_err();
    assert_eq!(code(missing), "expires_at_required");
    let unexpected = h
        .procedure
        .observe_renewal(
            "provider-token",
            &ObserveRenewalIn { outcome: Observation::Never, expires_at: Some(expires), observed_at: Utc::now() },
        )
        .await
        .unwrap_err();
    assert_eq!(code(unexpected), "expires_at_unexpected");
    let wrong_kind = h
        .procedure
        .observe_renewal(
            "cadence-token",
            &ObserveRenewalIn { outcome: Observation::Date, expires_at: Some(expires), observed_at: Utc::now() },
        )
        .await
        .unwrap_err();
    assert_eq!(code(wrong_kind), "not_provider");
    let forbidden = h
        .agent
        .observe_renewal("provider-token", &ObserveRenewalIn { outcome: Observation::Never, expires_at: None, observed_at: Utc::now() })
        .await
        .unwrap_err();
    assert_eq!(code(forbidden), "forbidden");
    assert_eq!(renewal(&h, "provider-token").await.observation, Some(Observation::Inactive), "a refused observation changes nothing");

    // A re-seed refreshes facts but never the observation columns.
    let mut reseeded = renewal_seed("provider-token", None, None);
    reseeded["expiry_kind"] = json!("provider");
    reseeded["title"] = json!("Renamed provider token");
    seed_renewals(&h, json!([reseeded])).await;
    let after = renewal(&h, "provider-token").await;
    assert_eq!(after.title, "Renamed provider token");
    assert_eq!(after.observation, Some(Observation::Inactive));
    assert_eq!(after.observed_at, inactive.observed_at);
    assert_eq!(after.last_good_at, inactive.last_good_at);
    assert_eq!(after.last_good_expires_at, inactive.last_good_expires_at);
}

#[tokio::test]
async fn renewals_stay_inside_the_hubs_company_and_keys_are_slugs() {
    let Some(h) = harness().await else { return };
    let seed = |renewals| serde_json::from_value::<bootstrap::Seed>(json!({ "renewals": renewals })).unwrap();
    let elsewhere = seed(json!([renewal_seed("other-company-token", Some(60), None)]));
    bootstrap::apply_seed(&h.hub, &elsewhere, "other-company").await.unwrap();
    seed_renewals(&h, json!([renewal_seed("own-token", Some(60), None)])).await;

    let keys: Vec<String> = h.op.renewals().await.unwrap().into_iter().map(|row| row.key).collect();
    assert!(keys.contains(&"own-token".to_string()));
    assert!(!keys.contains(&"other-company-token".to_string()), "another company's renewal leaked: {keys:?}");
    let titles: Vec<String> = h.op.today().await.unwrap().renewals.into_iter().map(|item| item.title).collect();
    assert!(!titles.contains(&"Token other-company-token".to_string()), "another company's renewal in Today");
    let missing = h.op.renewal_renewed("other-company-token", &RenewalRenewedIn { due_at: None }).await.unwrap_err();
    assert_eq!(code(missing), "not_found");

    for bad in ["linkedin/personal", "linkedin#personal", "Upper", ""] {
        let refused = bootstrap::apply_seed(&h.hub, &seed(json!([renewal_seed(bad, Some(60), None)])), "fixture-company").await;
        assert!(refused.is_err(), "{bad:?} must not be a renewal key");
    }
}

fn notification_in(key: &str, body: &str) -> RaiseNotificationIn {
    RaiseNotificationIn {
        source: None,
        dedupe_key: key.into(),
        severity: Severity::Warning,
        title: "Nightly failed".into(),
        body: body.into(),
        subject_type: Some("host".into()),
        subject_id: Some("atlas".into()),
        url: None,
    }
}

fn notification_raise(source: &str, key: &str) -> domain::notifications::Raise {
    domain::notifications::Raise {
        source: source.into(),
        dedupe_key: key.into(),
        severity: Severity::Warning,
        title: "Renewal due".into(),
        body: String::new(),
        subject_type: Some("renewal".into()),
        subject_id: Some("abc".into()),
        url: None,
    }
}

#[tokio::test]
async fn notifications_dedupe_ack_resolve_and_wake() {
    let Some(h) = harness().await else { return };

    // Raising the same condition twice is one row and one seq.
    let first = h.op.raise_notification(&notification_in("agent:nightly", "step failed")).await.unwrap();
    let again = h.op.raise_notification(&notification_in("agent:nightly", "step failed")).await.unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.seq, first.seq, "an unchanged condition touches nothing");
    assert_eq!(first.source, "agent", "source defaults to agent");
    let page = h.op.notifications(None, 0, true).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!((page.open, page.critical), (1, 0));
    assert_eq!(page.cursor, first.seq);
    assert!(page.items[0].open);
    assert_eq!(h.op.notifications(None, 0, false).await.unwrap().items.len(), 1, "open is the default state");

    // Acknowledging closes the row, moves the cursor and audits once.
    let after_ack = h.op.ack_notifications(&AckNotificationsIn { ids: vec![first.id], all: false }).await.unwrap();
    assert_eq!(after_ack.open, 0);
    assert!(after_ack.items.is_empty(), "an acknowledged row is not open");
    let acked = h.op.notifications(None, 0, true).await.unwrap().items.remove(0);
    assert!(acked.read_at.is_some() && !acked.open);
    assert!(acked.seq > first.seq, "the ack moves the cursor");
    let history = h.op.get::<Vec<HistoryEntry>>(&format!("/history/notification/{}", first.id)).await.unwrap();
    assert_eq!(history.iter().filter(|e| e.action == "notification.acked").count(), 1);
    assert_eq!(
        code(h.agent.ack_notifications(&AckNotificationsIn { ids: vec![], all: true }).await.unwrap_err()),
        "forbidden",
        "ack stays with the operator"
    );

    // A producer pass resolves what it no longer reports, and only its source.
    domain::notifications::raise(&h.hub, &notification_raise("today", "today:renewal:abc:due")).await.unwrap();
    assert_eq!(h.op.notifications(None, 0, false).await.unwrap().open, 1);
    assert_eq!(domain::notifications::sync(&h.hub, "today", vec![]).await.unwrap(), 1);
    let rows = h.op.notifications(None, 0, true).await.unwrap();
    let today = rows.items.iter().find(|n| n.dedupe_key == "today:renewal:abc:due").unwrap();
    assert!(today.resolved_at.is_some() && !today.open);
    let kept = rows.items.iter().find(|n| n.id == first.id).unwrap();
    assert!(kept.read_at.is_some() && kept.resolved_at.is_none(), "sync leaves other sources alone");

    // A changed condition reopens unread.
    let reopened = h.op.raise_notification(&notification_in("agent:nightly", "step failed again")).await.unwrap();
    assert_eq!(reopened.id, first.id);
    assert!(reopened.read_at.is_none() && reopened.resolved_at.is_none() && reopened.open);
    assert!(reopened.seq > kept.seq);

    // Snooze hides it from the open page, and only into the future.
    let snoozed =
        h.op.snooze_notification(reopened.id, &SnoozeNotificationIn { until: Utc::now() + chrono::Duration::hours(1) }).await.unwrap();
    assert!(snoozed.snoozed_until.is_some() && !snoozed.open);
    assert!(h.op.notifications(None, 0, false).await.unwrap().items.is_empty());
    let past =
        h.op.snooze_notification(reopened.id, &SnoozeNotificationIn { until: Utc::now() - chrono::Duration::hours(1) }).await.unwrap_err();
    assert_eq!(code(past), "past_until");
    let history = h.op.get::<Vec<HistoryEntry>>(&format!("/history/notification/{}", reopened.id)).await.unwrap();
    assert!(history.iter().any(|e| e.action == "notification.snoozed"));

    // The long poll: a request started first returns at once when a raise lands.
    let after = h.op.notifications(None, 0, true).await.unwrap().cursor;
    let wake = h.op.clone();
    let started = std::time::Instant::now();
    let waiter = tokio::spawn(async move { wake.notifications(Some(after), 5, true).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.op.raise_notification(&notification_in("agent:wake", "woke")).await.unwrap();
    let woke = waiter.await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "the long poll woke in {:?}", started.elapsed());
    assert!(woke.items.iter().any(|n| n.dedupe_key == "agent:wake"));
    assert!(woke.cursor > after);

    // Reserved keys and size limits are refusals, not 500s.
    for key in ["today:renewal:abc:due", "fleet:host:atlas", "hub:summary-stale"] {
        assert_eq!(code(h.op.raise_notification(&notification_in(key, "")).await.unwrap_err()), "reserved_key", "{key}");
    }
    let reserved_source = RaiseNotificationIn { source: Some("fleet".into()), ..notification_in("agent:x", "") };
    assert_eq!(code(h.op.raise_notification(&reserved_source).await.unwrap_err()), "reserved_key");
    let long_title = RaiseNotificationIn { title: "x".repeat(201), ..notification_in("agent:long", "") };
    assert_eq!(code(h.op.raise_notification(&long_title).await.unwrap_err()), "invalid_title");
    let big_body = RaiseNotificationIn { body: "x".repeat(16 * 1024 + 1), ..notification_in("agent:big", "") };
    assert_eq!(code(h.op.raise_notification(&big_body).await.unwrap_err()), "body_too_large");

    // Procedure tokens raise their own rows; ingest tokens do not.
    h.procedure.raise_notification(&notification_in("agent:procedure", "")).await.unwrap();
    assert_eq!(code(h.ingest.raise_notification(&notification_in("agent:ingest", "")).await.unwrap_err()), "forbidden");

    // Acknowledge all clears whatever is still open, but not a row snoozed into
    // the future: that one must resurface when its snooze lapses.
    let cleared = h.op.ack_notifications(&AckNotificationsIn { ids: vec![], all: true }).await.unwrap();
    assert_eq!(cleared.open, 0);
    assert!(cleared.items.is_empty());
    let rows = h.op.notifications(None, 0, true).await.unwrap();
    assert_eq!(rows.items.iter().filter(|n| n.open).count(), 0);
    assert!(rows.items.iter().find(|n| n.id == reopened.id).unwrap().read_at.is_none(), "ack all skips a snoozed row");
}

/// The `hub_post_summary` path: one row per day, replaced in place on a re-run.
#[tokio::test]
async fn summary_notification_replaces_its_day() {
    let Some(h) = harness().await else { return };

    let first = fleetix_hubd::mcp::summary_input("2026-11-15", "headline\nNeeds you: nothing", Severity::Info).unwrap();
    let posted = h.op.raise_notification(&first).await.unwrap();
    assert_eq!(posted.source, "summary", "the summary source is not reserved");
    assert_eq!(posted.dedupe_key, "summary:2026-11-15");
    assert_eq!(posted.title, "Daily summary 2026-11-15");

    // Same day, same text: one row, no seq bump, so no second popup.
    let unchanged = h.op.raise_notification(&first).await.unwrap();
    assert_eq!((unchanged.id, unchanged.seq), (posted.id, posted.seq), "an unchanged summary touches nothing");

    // A corrected re-run keeps the row and the key, moves the body and the seq.
    let second = fleetix_hubd::mcp::summary_input("2026-11-15", "headline changed", Severity::Warning).unwrap();
    let replaced = h.op.raise_notification(&second).await.unwrap();
    assert_eq!(replaced.id, posted.id);
    assert_eq!(replaced.dedupe_key, "summary:2026-11-15");
    assert_eq!(replaced.body, "headline changed");
    assert_eq!(replaced.severity, Severity::Warning);
    assert!(replaced.seq > posted.seq, "a changed summary is announced once");

    let rows = h.op.notifications(None, 0, true).await.unwrap().items;
    assert_eq!(rows.iter().filter(|row| row.dedupe_key.starts_with("summary:")).count(), 1);

    // The next day is a new row, and a malformed day never becomes a key.
    h.op.raise_notification(&fleetix_hubd::mcp::summary_input("2026-11-16", "next day", Severity::Info).unwrap()).await.unwrap();
    assert_eq!(h.op.notifications(None, 0, true).await.unwrap().items.len(), 2);
    assert!(fleetix_hubd::mcp::summary_input("2026-13-01", "bad", Severity::Info).is_err());
    assert!(fleetix_hubd::mcp::summary_input("2026-11-15x", "bad", Severity::Info).is_err());
}

#[tokio::test]
async fn notification_feed_never_skips_a_truncated_change() {
    let Some(h) = harness().await else { return };
    let op = actor(Role::Operator, "operator");
    let start = domain::notifications::page(&h.hub, &op, None, true, 200).await.unwrap().cursor;
    for key in ["agent:feed-a", "agent:feed-b", "agent:feed-c"] {
        h.op.raise_notification(&notification_in(key, "")).await.unwrap();
    }
    // A one-row window walks the changes oldest first and never jumps past one.
    let mut cursor = start;
    let mut seen = Vec::new();
    for _ in 0..3 {
        let page = domain::notifications::page(&h.hub, &op, Some(cursor), true, 1).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.cursor, page.items[0].seq, "the cursor stops at the last row delivered");
        seen.push(page.items[0].dedupe_key.clone());
        cursor = page.cursor;
    }
    assert_eq!(seen, ["agent:feed-a", "agent:feed-b", "agent:feed-c"]);
    let done = domain::notifications::page(&h.hub, &op, Some(cursor), true, 1).await.unwrap();
    assert!(done.items.is_empty() && done.cursor == cursor);
}

#[tokio::test]
async fn producer_pass_reopens_a_lapsed_snooze_and_tracks_summary_staleness() {
    let Some(h) = harness().await else { return };

    // A snoozed row is hidden; the producer pass announces it once the snooze lapses.
    let row = h.op.raise_notification(&notification_in("agent:snoozed", "wait")).await.unwrap();
    let snoozed = h.op.snooze_notification(row.id, &SnoozeNotificationIn { until: Utc::now() + chrono::Duration::hours(1) }).await.unwrap();
    assert!(!snoozed.open, "a snoozed row is not open");
    domain::sqlx::query("update notifications set snoozed_until = now() - interval '1 minute' where id = $1")
        .bind(row.id)
        .execute(&h.hub.db)
        .await
        .unwrap();

    // No summary has ever been written, so the hub source raises staleness.
    fleetix_hubd::notify_pass(&h.hub, None, Some(26)).await;
    let rows = h.op.notifications(None, 0, true).await.unwrap();
    let reopened = rows.items.iter().find(|n| n.id == row.id).expect("the snoozed row survives");
    assert!(reopened.snoozed_until.is_none() && reopened.open, "a lapsed snooze reopens");
    assert!(reopened.seq > snoozed.seq, "the reopen moves the feed cursor");
    let stale = rows.items.iter().find(|n| n.dedupe_key == "hub:summary-stale").expect("summary staleness is raised");
    assert!(stale.open && stale.severity == Severity::Warning && stale.source == "hub");

    // The day's summary resolves it on the next pass.
    domain::notifications::raise(&h.hub, &notification_raise("summary", "summary:2026-11-15")).await.unwrap();
    fleetix_hubd::notify_pass(&h.hub, None, Some(26)).await;
    let rows = h.op.notifications(None, 0, true).await.unwrap();
    let stale = rows.items.iter().find(|n| n.dedupe_key == "hub:summary-stale").unwrap();
    assert!(stale.resolved_at.is_some() && !stale.open, "a fresh summary resolves the raise");

    // With the rule unset the source syncs empty, so nothing stale can survive.
    fleetix_hubd::notify_pass(&h.hub, None, None).await;
    let rows = h.op.notifications(None, 0, true).await.unwrap();
    assert!(rows.items.iter().find(|n| n.dedupe_key == "hub:summary-stale").unwrap().resolved_at.is_some());
}

#[tokio::test]
async fn stale_observations_and_quiet_rows_never_hide_a_crossed_deadline() {
    let Some(h) = harness().await else { return };
    let mut provider = renewal_seed("crossed-token", None, None);
    provider["expiry_kind"] = json!("provider");
    // Many quiet rows: unknown cadence and never-expiring credentials.
    let mut rows = vec![provider];
    for i in 0..210 {
        let mut quiet = renewal_seed(&format!("quiet-{i:03}"), None, None);
        if i % 2 == 0 {
            quiet["expiry_kind"] = json!("never");
        }
        rows.push(quiet);
    }
    seed_renewals(&h, serde_json::Value::Array(rows)).await;

    // A fresh provider answer: the credential already expired.
    let now = Utc::now();
    let expired = h
        .procedure
        .observe_renewal(
            "crossed-token",
            &ObserveRenewalIn { outcome: Observation::Date, expires_at: Some(now - chrono::Duration::days(1)), observed_at: now },
        )
        .await
        .unwrap();
    assert_eq!(expired.status, "overdue");

    // An older answer (replayed or delayed) is refused and changes nothing.
    for observed_at in [now, now - chrono::Duration::hours(49), now - chrono::Duration::hours(1)] {
        let stale = h
            .procedure
            .observe_renewal("crossed-token", &ObserveRenewalIn { outcome: Observation::Never, expires_at: None, observed_at })
            .await
            .unwrap_err();
        assert_eq!(code(stale), "observation_out_of_order");
    }
    assert_eq!(renewal(&h, "crossed-token").await.status, "overdue");

    // More than 200 quiet rows never push the overdue renewal out of Today.
    let today = h.op.today().await.unwrap();
    let item = today.renewals.iter().find(|item| item.subject_id == expired.id).expect("overdue renewal is in Today");
    assert_eq!(item.status, "overdue");
    assert_eq!(today.renewals.first().map(|item| item.subject_id), Some(expired.id), "the most urgent row comes first");
}
