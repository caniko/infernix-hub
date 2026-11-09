//! Transactional outbox. Commands are written with the state change that
//! causes them; the worker delivers them. Retrying a delivery re-sends the
//! same command (same idempotency key) — it never re-runs a workflow stage.

use serde_json::Value;
use uuid::Uuid;

use crate::{Actor, DResult};

pub const PAPERCLIP_CREATE_ISSUE: &str = "paperclip.create_issue";
pub const PAPERCLIP_CREATE_APPROVAL: &str = "paperclip.create_approval";
pub const WINDMILL_RUN_GENERATION: &str = "windmill.run_generation";
pub const WINDMILL_SCHEDULE_PUBLICATION: &str = "windmill.schedule_publication";
pub const PUBLISHER_SEND_REPLY: &str = "publisher.send_reply";

pub async fn enqueue(tx: &mut sqlx::PgConnection, actor: &Actor, topic: &str, idempotency_key: &str, payload: Value) -> DResult<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        "insert into outbox (id, topic, idempotency_key, payload, correlation_id) values ($1, $2, $3, $4, $5)
         on conflict (idempotency_key) do update set idempotency_key = excluded.idempotency_key
         returning id",
    )
    .bind(Uuid::new_v4())
    .bind(topic)
    .bind(idempotency_key)
    .bind(payload)
    .bind(&actor.correlation_id)
    .fetch_one(tx)
    .await?;
    Ok(id)
}

#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: Uuid,
    pub topic: String,
    pub idempotency_key: String,
    pub payload: Value,
    pub attempts: i32,
    pub correlation_id: Option<String>,
}

/// Claim due rows. `skip locked` lets several workers share the table.
pub async fn claim_due(tx: &mut sqlx::PgConnection, limit: i64) -> DResult<Vec<OutboxRow>> {
    let rows: Vec<(Uuid, String, String, Value, i32, Option<String>)> = sqlx::query_as(
        "select id, topic, idempotency_key, payload, attempts, correlation_id from outbox
         where state = 'pending' and next_attempt_at <= now()
         order by next_attempt_at limit $1 for update skip locked",
    )
    .bind(limit)
    .fetch_all(tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, topic, idempotency_key, payload, attempts, correlation_id)| OutboxRow {
            id,
            topic,
            idempotency_key,
            payload,
            attempts,
            correlation_id,
        })
        .collect())
}

pub async fn mark_delivered(tx: &mut sqlx::PgConnection, id: Uuid) -> DResult<()> {
    sqlx::query("update outbox set state = 'delivered', delivered_at = now(), attempts = attempts + 1, last_error = null where id = $1")
        .bind(id)
        .execute(tx)
        .await?;
    Ok(())
}

/// Back off exponentially (capped at 10 min); dead-letter after `max_attempts`.
pub async fn mark_failed(tx: &mut sqlx::PgConnection, id: Uuid, error: &str, retryable: bool, max_attempts: i32) -> DResult<()> {
    sqlx::query(
        "update outbox set attempts = attempts + 1, last_error = $2,
           state = case when not $3 or attempts + 1 >= $4 then 'dead' else 'pending' end,
           next_attempt_at = now() + least(interval '10 minutes', interval '2 seconds' * power(2, attempts))
         where id = $1",
    )
    .bind(id)
    .bind(error)
    .bind(retryable)
    .bind(max_attempts)
    .execute(tx)
    .await?;
    Ok(())
}

/// Postpone a delivery without spending a retry (e.g. waiting for GPU
/// admission). Returns true when the reason changed.
pub async fn defer(tx: &mut sqlx::PgConnection, id: Uuid, secs: i64, reason: &str) -> DResult<bool> {
    let changed: bool = sqlx::query_scalar(
        "update outbox o set next_attempt_at = now() + make_interval(secs => $2), last_error = $3
         from (select last_error from outbox where id = $1) prev where o.id = $1
         returning prev.last_error is distinct from $3",
    )
    .bind(id)
    .bind(secs as f64)
    .bind(reason)
    .fetch_one(tx)
    .await?;
    Ok(changed)
}
