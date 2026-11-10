//! End-to-end behaviour over HTTP with fixture adapters and a real
//! PostgreSQL. Skips (passes trivially, with a note) when
//! `HUB_TEST_DATABASE_URL` is unset — see `scripts/dev-db.sh`.

use std::time::Duration;

use chrono::Utc;
use contracts::client::{ClientError, HubClient};
use contracts::*;
use infernix_hubd::bootstrap::{self, Fixtures, Settings};
use integrations::fixture::PublishMode;
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
        axum::serve(listener, infernix_hubd::router(state)).await.unwrap();
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
    let doc: serde_json::Value = serde_json::from_str(infernix_hubd::OPENAPI).unwrap();
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
