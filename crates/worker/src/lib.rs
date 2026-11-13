//! Integration worker: delivers outbox commands, reconciles external state
//! through authoritative reads, and listens to ComfyUI progress. It never
//! decides workflow stages on its own; every state change goes through
//! `hub-domain`.

use std::time::Duration;

use domain::outbox::Delivery;
use domain::ports::{ApprovalRequest, GenerationJob, IssueRequest, PortError, PublicationTimer};
use domain::{DResult, Hub, engagement, outbox, production, release};
use serde_json::Value;
use uuid::Uuid;

const MAX_ATTEMPTS: i32 = 12;

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

/// A pipeline submission row the outbox dead-letters can never be delivered
/// again. End its attempt in the same transaction that dead-letters the row, or
/// the generation waits forever for a run nothing will follow. The submission
/// claim stays on the attempt, so a run this row may already have started is
/// never submitted a second time.
async fn dead_letter_pipeline(tx: &mut domain::sqlx::PgConnection, row: &outbox::OutboxRow, error: &str) -> DResult<()> {
    if row.topic != production::PIPELINE_RUN {
        return Ok(());
    }
    let Some(attempt_id) = uuid_field(&row.payload, "attempt_id") else { return Ok(()) };
    production::fail_dead_pipeline_submission(tx, attempt_id, error).await
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
                // Frozen when the brief was created (delegated producer only).
                project_id: p["project_id"].as_str().map(String::from),
                assignee_agent_id: p["assignee_agent_id"].as_str().map(String::from),
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
            // Replay after a crash that lost only the outbox commit: the
            // approval was created and recorded; do not create a second one.
            match domain::sqlx::query_scalar::<_, bool>("select paperclip_approval_id is not null from release_revisions where id = $1")
                .bind(release_id)
                .fetch_optional(&hub.db)
                .await
            {
                Ok(Some(true)) => return Delivery::Done,
                Ok(_) => {}
                Err(e) => return Delivery::Retry(e.to_string()),
            }
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
        production::PIPELINE_RUN => {
            let job: production::PipelineJob = match serde_json::from_value(p.clone()) {
                Ok(j) => j,
                Err(e) => return Delivery::Dead(format!("bad payload: {e}")),
            };
            match production::deliver_pipeline(hub, &job).await {
                Ok(production::PipelineDelivery::Done) => Delivery::Done,
                Ok(production::PipelineDelivery::Retry(e)) => Delivery::Retry(e),
                Ok(production::PipelineDelivery::Defer(wait)) => Delivery::Defer(wait),
                Ok(production::PipelineDelivery::Dead(e)) => Delivery::Dead(e),
                Err(e) => Delivery::Dead(e.to_string()),
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
        other => match hub.topics.get(other) {
            Some(handler) => handler.deliver(hub, row).await,
            None => Delivery::Dead(format!("unknown topic {other}")),
        },
    }
}

/// Deliver due outbox rows, one row per transaction.
// ponytail: the row lock is held across the remote call; fine for one worker
// and small batches, switch to a lease column if delivery gets slow. A crash
// between a non-idempotent call and its record still replays it; approvals and
// generations check their recorded effect first, which closes the common case.
pub async fn drain_outbox(hub: &Hub) -> DResult<usize> {
    let mut n = 0;
    loop {
        let mut tx = hub.db.begin().await?;
        let rows = outbox::claim_due(&mut tx, 1).await?;
        let Some(row) = rows.into_iter().next() else { return Ok(n) };
        let span = tracing::info_span!("outbox", topic = %row.topic, correlation_id = row.correlation_id.as_deref().unwrap_or(""));
        let _g = span.enter();
        let gen_id = match row.topic.as_str() {
            outbox::WINDMILL_RUN_GENERATION => row.payload["generation_request_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
            production::PIPELINE_RUN => row.payload["request"]["generation_request_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
            _ => None,
        };
        // The ComfyUI procedure records its start; a pipeline submission
        // guards itself in `deliver_pipeline`.
        if row.topic == outbox::WINDMILL_RUN_GENERATION
            && let Some(gen_id) = gen_id
            && let Some((done, why)) = production::dispatch_blocker(hub, gen_id).await?
        {
            tracing::info!(%why, "generation command not sent");
            if done {
                outbox::mark_delivered(&mut tx, row.id).await?
            } else {
                outbox::mark_failed(&mut tx, row.id, &why, false, MAX_ATTEMPTS).await?
            }
            tx.commit().await?;
            n += 1;
            continue;
        }
        if let Some(gen_id) = gen_id
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
                outbox::mark_failed(&mut tx, row.id, &e, true, MAX_ATTEMPTS).await?;
                // This delivery spent the last retry: the row is dead now.
                if row.attempts + 1 >= MAX_ATTEMPTS {
                    dead_letter_pipeline(&mut tx, &row, &e).await?;
                }
            }
            Delivery::Defer(wait) => {
                tracing::info!(secs = wait.as_secs(), "outbox delivery deferred by its handler");
                outbox::defer(&mut tx, row.id, wait.as_secs() as i64, "deferred by topic handler").await?;
            }
            Delivery::Dead(e) => {
                tracing::error!(error = %e, "outbox delivery dead-lettered");
                outbox::mark_failed(&mut tx, row.id, &e, false, MAX_ATTEMPTS).await?;
                dead_letter_pipeline(&mut tx, &row, &e).await?;
            }
        }
        tx.commit().await?;
        n += 1;
    }
}

/// One reconciliation pass over every open external receipt.
pub async fn reconcile(hub: &Hub) -> DResult<()> {
    // Local repairs first: pipeline deadlines and stranded generations never
    // wait behind a remote read.
    production::reconcile_pipeline_deadlines(hub).await?;
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
    // Procedure and pipeline polls read a bounded random batch (20 each, 5 s per
    // call), so one stalled backend cannot hold the pass for long.
    production::reconcile_procedures(hub).await
}

/// Outbox + reconcile, for tests and the `dev` loop.
pub async fn tick(hub: &Hub) -> DResult<()> {
    drain_outbox(hub).await?;
    reconcile(hub).await
}

/// Long-running worker: periodic ticks plus `/ws` progress listeners
/// (reconnecting; history reconciliation stays authoritative). An engine with
/// `capabilities.on_demand` (socket-activated, idle-exits) only gets a
/// job-scoped subscription while one of its prompts is open.
pub async fn run(hub: Hub, every: Duration) {
    let engines: Vec<(String, String, bool)> =
        domain::sqlx::query_as("select id, base_url, coalesce(capabilities ->> 'on_demand', 'false') = 'true' from engine_instances")
            .fetch_all(&hub.db)
            .await
            .unwrap_or_default();
    for (id, url, on_demand) in engines {
        let hub = hub.clone();
        if on_demand {
            tokio::spawn(async move { listen_on_demand(hub, id, url, every).await });
            continue;
        }
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                let (tx, mut rx) = tokio::sync::mpsc::channel(256);
                let comfy = hub.comfy.clone();
                let url2 = url.clone();
                let sub = tokio::spawn(async move { comfy.subscribe(&url2, &production::comfy_client_id(), tx).await });
                while let Some(ev) = rx.recv().await {
                    backoff = Duration::from_secs(1);
                    if let Err(e) = apply_progress(&hub, &ev).await {
                        tracing::warn!(error = %e, "progress update failed");
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

/// Record one progress event and reconcile on a terminal hint.
async fn apply_progress(hub: &Hub, ev: &domain::ports::ProgressEvent) -> DResult<()> {
    let Some(prompt_id) = production::record_progress(hub, ev).await? else { return Ok(()) };
    if let Some(p) = production::open_prompts(hub).await?.into_iter().find(|p| p.prompt_id == prompt_id) {
        production::reconcile_prompt(hub, &p).await?;
    }
    Ok(())
}

/// A spawned subscription that is aborted when its owner goes away, including
/// when the owner itself is aborted. A bare `JoinHandle` would leave the socket
/// blocked and the engine unable to idle-exit.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Subscribe per open prompt, never for the process lifetime: the socket closes
/// as soon as its prompt reaches a terminal state, so the engine may idle-exit.
async fn listen_on_demand(hub: Hub, engine_id: String, url: String, every: Duration) {
    let mut subscribed: std::collections::BTreeMap<String, AbortOnDrop<()>> = std::collections::BTreeMap::new();
    loop {
        let open: Vec<String> = production::open_prompts(&hub)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.engine_id == engine_id)
            .map(|p| p.prompt_id)
            .collect();
        // Dropping a guard aborts its socket: that is what lets the engine idle.
        subscribed.retain(|prompt_id, _| open.contains(prompt_id));
        for prompt_id in open {
            if subscribed.contains_key(&prompt_id) {
                continue;
            }
            let hub = hub.clone();
            let url = url.clone();
            let task = tokio::spawn(async move {
                let (tx, mut rx) = tokio::sync::mpsc::channel(64);
                let comfy = hub.comfy.clone();
                // Owning the spawned socket keeps it abortable: dropping this
                // guard (including by aborting the task) closes the connection.
                let _sub = AbortOnDrop(tokio::spawn(async move { comfy.subscribe(&url, &production::comfy_client_id(), tx).await }));
                while let Some(ev) = rx.recv().await {
                    if let Err(e) = apply_progress(&hub, &ev).await {
                        tracing::warn!(error = %e, "progress update failed");
                    }
                }
            });
            subscribed.insert(prompt_id, AbortOnDrop(task));
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The bug this guards: aborting the listener left its separately spawned
    /// subscription blocked on the socket forever.
    #[tokio::test]
    async fn aborting_a_listener_aborts_its_subscription() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Flag(dropped.clone());
        let sub = tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await;
        });
        // Drop the owner the way `subscribed.retain` does when a prompt ends.
        let owner = AbortOnDrop(sub);
        drop(owner);
        for _ in 0..200 {
            if dropped.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the subscription outlived its listener");
    }
}
