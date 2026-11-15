//! Opportunities: funding calls, fellowships, prizes, venues and programmes.
//!
//! Ingest is idempotent on `(company_id, source, external_id)` and refreshes
//! facts only; the operator's decision is the only writer of `state`. `expired`
//! is derived on read (a passed deadline while the call is still open), so no
//! timer rewrites rows.

use contracts::{Opportunity, OpportunityDecisionIn, OpportunityIn, OpportunityState, Role};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR, audit, decode, decode_one, invalid};

/// This endpoint alone accepts agents: a scout token files calls without
/// widening the other ingest endpoints.
const INGESTERS: &[Role] = &[Role::Ingest, Role::Procedure, Role::Operator, Role::Agent];

/// Rows are decoded from the contract field names. `expired` coalesces the SQL
/// three-valued result so a rolling deadline is `false`, not null.
const SELECT: &str = "select to_jsonb(t) from (
    select o.id, o.company_id, o.persona_id, o.source, o.external_id, o.kind, o.title, o.funder, o.url,
      o.deadline, o.amount, o.eligibility, o.fit, o.evidence, o.state, o.note, o.paperclip_issue_id,
      coalesce(o.deadline < now() and o.state in ('new', 'shortlisted', 'drafting'), false) as expired,
      o.created_at, o.updated_at
    from opportunities o";

pub async fn ingest(hub: &Hub, actor: &Actor, input: OpportunityIn) -> DResult<Opportunity> {
    actor.require(INGESTERS)?;
    if input.source.trim().is_empty()
        || input.external_id.trim().is_empty()
        || input.title.trim().is_empty()
        || input.funder.trim().is_empty()
        || input.url.trim().is_empty()
    {
        return Err(invalid("invalid_opportunity", "source, external_id, title, funder and url are required"));
    }
    if input.evidence.is_empty() {
        return Err(invalid("missing_evidence", "at least one evidence entry (url and quote) is required"));
    }
    let mut tx = hub.db.begin().await?;
    let (id, created): (Uuid, bool) = sqlx::query_as(
        "insert into opportunities
           (id, company_id, persona_id, source, external_id, kind, title, funder, url, deadline, amount, eligibility, fit, evidence)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         on conflict (company_id, source, external_id) do update set
           persona_id = coalesce(excluded.persona_id, opportunities.persona_id),
           kind = excluded.kind, title = excluded.title, funder = excluded.funder, url = excluded.url,
           deadline = excluded.deadline, amount = excluded.amount, eligibility = excluded.eligibility,
           fit = excluded.fit, evidence = excluded.evidence, updated_at = now()
         returning id, (xmax = 0)",
    )
    .bind(Uuid::new_v4())
    .bind(&hub.config.company_id)
    .bind(input.persona_id)
    .bind(&input.source)
    .bind(&input.external_id)
    .bind(input.kind.as_str())
    .bind(&input.title)
    .bind(&input.funder)
    .bind(&input.url)
    .bind(input.deadline)
    .bind(&input.amount)
    .bind(&input.eligibility)
    .bind(&input.fit)
    .bind(serde_json::to_value(&input.evidence).unwrap_or_default())
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        if created { "opportunity.ingested" } else { "opportunity.reingested" },
        "opportunity",
        id,
        json!({ "source": input.source, "external_id": input.external_id }),
    )
    .await?;
    let row = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(row)
}

async fn one(tx: &mut sqlx::PgConnection, id: Uuid) -> DResult<Opportunity> {
    let row: Option<Value> = sqlx::query_scalar(&format!("{SELECT} where o.id = $1) t")).bind(id).fetch_optional(tx).await?;
    decode_one(row.ok_or(DomainError::NotFound("opportunity"))?)
}

pub async fn list(hub: &Hub, actor: &Actor, state: Option<OpportunityState>, due_within_days: Option<i32>) -> DResult<Vec<Opportunity>> {
    actor.require(crate::ANY_READER)?;
    if due_within_days.is_some_and(|days| days <= 0) {
        return Err(invalid("invalid_due_within_days", "due_within_days must be a positive number of days"));
    }
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "{SELECT}
         where ($1::text is null or o.state = $1)
           and ($2::int is null or o.deadline <= now() + $2::int * interval '1 day')
         order by o.deadline nulls last, o.created_at desc limit 200) t"
    ))
    .bind(state.map(OpportunityState::as_str))
    .bind(due_within_days)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

pub async fn get(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<Opportunity> {
    actor.require(crate::ANY_READER)?;
    let row: Option<Value> = sqlx::query_scalar(&format!("{SELECT} where o.id = $1) t")).bind(id).fetch_optional(&hub.db).await?;
    decode_one(row.ok_or(DomainError::NotFound("opportunity"))?)
}

/// Operator-only. Every change lands in the audit log; ingest can never move
/// the state.
pub async fn decide(hub: &Hub, actor: &Actor, id: Uuid, input: OpportunityDecisionIn) -> DResult<Opportunity> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let current: Option<String> =
        sqlx::query_scalar("select state from opportunities where id = $1 for update").bind(id).fetch_optional(&mut *tx).await?;
    let current = current.ok_or(DomainError::NotFound("opportunity"))?;
    sqlx::query(
        "update opportunities
         set state = $2, note = coalesce($3, note), paperclip_issue_id = coalesce($4, paperclip_issue_id), updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(input.state.as_str())
    .bind(&input.note)
    .bind(&input.paperclip_issue_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        "opportunity.decided",
        "opportunity",
        id,
        json!({ "from": current, "state": input.state, "note": input.note, "paperclip_issue_id": input.paperclip_issue_id }),
    )
    .await?;
    let row = one(&mut tx, id).await?;
    tx.commit().await?;
    Ok(row)
}
