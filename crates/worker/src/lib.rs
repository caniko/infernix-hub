//! Integration worker: delivers outbox commands, reconciles external state
//! through authoritative reads, and listens to ComfyUI progress. It never
//! decides workflow stages on its own; every state change goes through
//! `hub-domain`.

use std::time::Duration;

use domain::ports::{ApprovalRequest, GenerationJob, IssueRequest, PortError, PublicationTimer};
use domain::{DResult, Hub, engagement, outbox, production, release};
use serde_json::Value;
use uuid::Uuid;

const MAX_ATTEMPTS: i32 = 12;

#[derive(Debug)]
enum Delivery {
    Done,
    Retry(String),
    Dead(String),
}

fn classify(e: PortError, ambiguous_retry_safe: bool) -> Delivery {
    match e {
        PortError::Unreachable(m) => Delivery::Retry(m),
        PortError::Ambiguous(m) if ambiguous_retry_safe => Delivery::Retry(format!("ambiguous: {m}")),
        PortError::Ambiguous(m) => Delivery::Dead(format!("outcome unknown, reconcile before retrying: {m}")),
        PortError::Rejected { status, message } if status >= 500 || status == 429 => Delivery::Retry(format!("{status}: {message}")),
        other => Delivery::Dead(other.to_string()),
    }
}

fn uuid_field(payload: &Value, field: &str) -> Option<Uuid> {
    payload[field].as_str().and_then(|s| Uuid::parse_str(s).ok())
}

async fn deliver(hub: &Hub, row: &outbox::OutboxRow) -> Delivery {
    let p = &row.payload;
    match row.topic.as_str() {
        outbox::PAPERCLIP_CREATE_ISSUE => {
            let Some(content) = uuid_field(p, "content_item_id") else { return Delivery::Dead("bad payload".into()) };
            let req = IssueRequest {
                company_id: p["company_id"].as_str().unwrap_or_default().into(),
                title: p["title"].as_str().unwrap_or_default().into(),
                description: p["description"].as_str().unwrap_or_default().into(),
                origin_key: p["origin_key"].as_str().unwrap_or_default().into(),
            };
            // Retrying is safe: adapters dedupe on origin_key.
            match hub.paperclip.create_issue(&req).await {
                Ok(issue) => {
                    match domain::sqlx::query("update content_items set paperclip_issue_id = $2, paperclip_issue_url = $3 where id = $1")
                        .bind(content)
                        .bind(&issue.id)
                        .bind(&issue.url)
                        .execute(&hub.db)
                        .await
                    {
                        Ok(_) => Delivery::Done,
                        Err(e) => Delivery::Retry(e.to_string()),
                    }
                }
                Err(e) => classify(e, true),
            }
        }
        outbox::PAPERCLIP_CREATE_APPROVAL => {
            let Some(release_id) = uuid_field(p, "release_revision_id") else { return Delivery::Dead("bad payload".into()) };
            let req = ApprovalRequest {
                company_id: p["company_id"].as_str().unwrap_or_default().into(),
                title: p["title"].as_str().unwrap_or_default().into(),
                summary: p["summary"].as_str().unwrap_or_default().into(),
                issue_id: p["issue_id"].as_str().map(String::from),
                release_revision_id: release_id.to_string(),
                payload_hash: p["payload_hash"].as_str().unwrap_or_default().into(),
                actions: p["actions"].clone(),
            };
            match hub.paperclip.create_approval(&req).await {
                Ok(a) => match release::record_approval_created(hub, release_id, &a.id).await {
                    Ok(()) => Delivery::Done,
                    Err(e) => Delivery::Retry(e.to_string()),
                },
                // Paperclip 0.3.1 approvals have no idempotency key: an
                // ambiguous create is dead-lettered and the operator links the
                // approval that exists (`link-approval` verifies its hash).
                Err(e) => classify(e, false),
            }
        }
        outbox::WINDMILL_RUN_GENERATION => {
            let job: GenerationJob = match serde_json::from_value(p.clone()) {
                Ok(j) => j,
                Err(e) => return Delivery::Dead(format!("bad payload: {e}")),
            };
            let Ok(gen_id) = Uuid::parse_str(&job.generation_request_id) else { return Delivery::Dead("bad payload".into()) };
            // A second procedure run could resubmit prompts; on an ambiguous
            // start we stop and let prompt reconciliation find the work.
            match hub.windmill.run_generation(&job).await {
                Ok(job_id) => match production::record_dispatched(hub, gen_id, &job_id).await {
                    Ok(()) => Delivery::Done,
                    Err(e) => Delivery::Dead(format!("procedure {job_id} started but not recorded: {e}")),
                },
                Err(e) => classify(e, false),
            }
        }
        outbox::WINDMILL_SCHEDULE_PUBLICATION => {
            let timer: PublicationTimer = match serde_json::from_value(p.clone()) {
                Ok(t) => t,
                Err(e) => return Delivery::Dead(format!("bad payload: {e}")),
            };
            // Duplicate timers converge on the same intent, so retrying is safe.
            match hub.windmill.schedule_publication(&timer).await {
                Ok(job_id) => {
                    tracing::info!(intent = %timer.intent_id, rev = timer.schedule_revision, %job_id, "publication timer armed");
                    Delivery::Done
                }
                Err(e) => classify(e, true),
            }
        }
        outbox::PUBLISHER_SEND_REPLY => {
            let Some(draft) = uuid_field(p, "draft_id") else { return Delivery::Dead("bad payload".into()) };
            match engagement::deliver_reply(hub, draft).await {
                Ok(engagement::ReplyDelivery::Retry(e)) => Delivery::Retry(e),
                Ok(other) => {
                    tracing::info!(%draft, result = ?other, "reply delivery");
                    Delivery::Done
                }
                Err(e) => Delivery::Retry(e.to_string()),
            }
        }
        other => Delivery::Dead(format!("unknown topic {other}")),
    }
}

/// Deliver due outbox rows, one row per transaction.
// ponytail: the row lock is held across the remote call; fine for one worker
// and small batches, switch to a lease column if delivery gets slow.
pub async fn drain_outbox(hub: &Hub) -> DResult<usize> {
    let mut n = 0;
    loop {
        let mut tx = hub.db.begin().await?;
        let rows = outbox::claim_due(&mut tx, 1).await?;
        let Some(row) = rows.into_iter().next() else { return Ok(n) };
        let span = tracing::info_span!("outbox", topic = %row.topic, correlation_id = row.correlation_id.as_deref().unwrap_or(""));
        let _g = span.enter();
        if row.topic == outbox::WINDMILL_RUN_GENERATION
            && let Some(gen_id) = row.payload["generation_request_id"].as_str().and_then(|s| Uuid::parse_str(s).ok())
            && let production::Admission::Defer(reason) = production::admission(hub, gen_id).await?
        {
            if outbox::defer(&mut tx, row.id, 5, &reason).await? {
                tracing::info!(%reason, "generation awaiting GPU admission");
                domain::sqlx::query(
                    "insert into audit_log (actor, action, subject_type, subject_id, detail) values ('system:worker', 'generation.awaiting_admission', 'generation', $1, $2)",
                )
                .bind(gen_id.to_string())
                .bind(serde_json::json!({ "reason": reason }))
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            n += 1;
            continue;
        }
        match deliver(hub, &row).await {
            Delivery::Done => outbox::mark_delivered(&mut tx, row.id).await?,
            Delivery::Retry(e) => {
                tracing::warn!(error = %e, attempts = row.attempts, "outbox delivery will retry");
                outbox::mark_failed(&mut tx, row.id, &e, true, MAX_ATTEMPTS).await?
            }
            Delivery::Dead(e) => {
                tracing::error!(error = %e, "outbox delivery dead-lettered");
                outbox::mark_failed(&mut tx, row.id, &e, false, MAX_ATTEMPTS).await?
            }
        }
        tx.commit().await?;
        n += 1;
    }
}

/// One reconciliation pass over every open external receipt.
pub async fn reconcile(hub: &Hub) -> DResult<()> {
    for p in production::open_prompts(hub).await? {
        if let Err(e) = production::reconcile_prompt(hub, &p).await {
            tracing::warn!(prompt_id = %p.prompt_id, error = %e, "prompt reconcile failed");
        }
    }
    for (release_id, approval_id) in release::releases_in_review(hub).await? {
        match hub.paperclip.get_approval(&approval_id).await {
            Ok(view) => release::apply_approval(hub, release_id, &view).await?,
            Err(e) => tracing::warn!(%approval_id, error = %e, "approval read failed"),
        }
    }
    for (intent, account, key) in release::intents_to_reconcile(hub).await? {
        release::reconcile_intent(hub, intent, &account, &key).await?;
    }
    engagement::reconcile_replies(hub).await?;
    Ok(())
}

/// Outbox + reconcile, for tests and the `dev` loop.
pub async fn tick(hub: &Hub) -> DResult<()> {
    drain_outbox(hub).await?;
    reconcile(hub).await
}

/// Long-running worker: periodic ticks plus a `/ws` progress listener per
/// engine instance (reconnecting; history reconciliation stays authoritative).
pub async fn run(hub: Hub, every: Duration) {
    let engines: Vec<(String, String)> =
        domain::sqlx::query_as("select id, base_url from engine_instances").fetch_all(&hub.db).await.unwrap_or_default();
    for (id, url) in engines {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                let (tx, mut rx) = tokio::sync::mpsc::channel(256);
                let comfy = hub.comfy.clone();
                let url2 = url.clone();
                let sub = tokio::spawn(async move { comfy.subscribe(&url2, &production::comfy_client_id(), tx).await });
                while let Some(ev) = rx.recv().await {
                    backoff = Duration::from_secs(1);
                    match production::record_progress(&hub, &ev).await {
                        Ok(Some(prompt_id)) => {
                            if let Ok(open) = production::open_prompts(&hub).await
                                && let Some(p) = open.iter().find(|p| p.prompt_id == prompt_id)
                            {
                                let _ = production::reconcile_prompt(&hub, p).await;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "progress update failed"),
                    }
                }
                let result = sub.await;
                tracing::info!(engine = %id, ?result, "progress socket closed; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        });
    }
    loop {
        if let Err(e) = tick(&hub).await {
            tracing::error!(error = %e, "worker tick failed");
        }
        tokio::time::sleep(every).await;
    }
}
