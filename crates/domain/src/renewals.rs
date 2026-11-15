//! Renewals: credential and account deadlines. The seed owns the facts
//! (title, procedure, guide URLs, cadence, warn window) and the operator owns
//! the clock: a re-seed never moves `due_at` or `renewed_at`.

use contracts::{Renewal, RenewalRenewedIn};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR, audit, decode, decode_one, invalid};

const SELECT: &str = "select to_jsonb(t) from (
    select r.id, r.company_id, r.key, r.account_id, r.title, r.procedure, r.urls,
      r.cadence_days, r.warn_days, r.due_at, r.renewed_at, r.created_at, r.updated_at
    from renewals r";

pub async fn list(hub: &Hub, actor: &Actor) -> DResult<Vec<Renewal>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> =
        sqlx::query_scalar(&format!("{SELECT} where r.company_id = $1 order by r.due_at nulls first, r.title limit 200) t"))
            .bind(&hub.config.company_id)
            .fetch_all(&hub.db)
            .await?;
    decode(rows)
}

/// Operator-only. An explicit due date wins; otherwise the stored cadence
/// starts the next clock. Neither is an error: the operator must say when.
pub async fn renewed(hub: &Hub, actor: &Actor, key: &str, input: RenewalRenewedIn) -> DResult<Renewal> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let cadence: Option<Option<i32>> =
        sqlx::query_scalar("select cadence_days from renewals where company_id = $1 and key = $2 for update")
            .bind(&hub.config.company_id)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    let cadence = cadence.ok_or(DomainError::NotFound("renewal"))?;
    if input.due_at.is_none() && cadence.is_none() {
        return Err(invalid("due_required", "renewal has no cadence_days; pass due_at"));
    }
    let id: Uuid = sqlx::query_scalar(
        "update renewals
         set renewed_at = now(), due_at = coalesce($3, now() + make_interval(days => cadence_days)), updated_at = now()
         where company_id = $1 and key = $2
         returning id",
    )
    .bind(&hub.config.company_id)
    .bind(key)
    .bind(input.due_at)
    .fetch_one(&mut *tx)
    .await?;
    audit(&mut tx, actor, "renewal.renewed", "renewal", id, json!({ "key": key, "due_at": input.due_at })).await?;
    let row: Value = sqlx::query_scalar(&format!("{SELECT} where r.id = $1) t")).bind(id).fetch_one(&mut *tx).await?;
    let row = decode_one(row)?;
    tx.commit().await?;
    Ok(row)
}
