//! Opportunities: funding calls, fellowships, prizes, venues and programmes.
//!
//! Ingest is idempotent on `(company_id, source, external_id)` and refreshes
//! facts only; the operator's decision is the only writer of `state`. `expired`
//! is derived on read (a passed deadline while the call is still open), so no
//! timer rewrites rows.

use contracts::{Opportunity, OpportunityDecisionIn, OpportunityIn, OpportunityState, Role, TodayItem};
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

pub(crate) fn steward_grant<'a>(hub: &'a Hub, actor: &Actor) -> Option<&'a crate::intel::IntelGrant> {
    hub.config.intel.grant(actor).ok().filter(|g| actor.principal.role == Role::Member && g.can_manage_opportunities)
}

fn scope<'a>(hub: &'a Hub, actor: &Actor, legacy: &[Role]) -> DResult<Option<Vec<&'a str>>> {
    if let Some(grant) = steward_grant(hub, actor) {
        return Ok(Some(grant.authorized_companies().collect()));
    }
    actor.require(legacy)?;
    if actor.principal.role == Role::Agent
        && let Ok(grant) = hub.config.intel.grant(actor)
    {
        return Ok(Some(grant.authorized_companies().collect()));
    }
    Ok((actor.principal.role != Role::Operator).then(|| vec![hub.config.company_id.as_str()]))
}

pub(crate) fn read_scope<'a>(hub: &'a Hub, actor: &Actor) -> DResult<Option<Vec<&'a str>>> {
    scope(hub, actor, crate::ANY_READER)
}

pub async fn ingest(hub: &Hub, actor: &Actor, input: OpportunityIn) -> DResult<Opportunity> {
    let companies = scope(hub, actor, INGESTERS)?;
    let company = input.company_id.as_deref().unwrap_or(&hub.config.company_id);
    let allowed = companies.as_ref().is_none_or(|companies| companies.contains(&company));
    if !allowed {
        return Err(DomainError::Forbidden("caller may not ingest opportunities for this company".into()));
    }
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
    if let Some(persona) = input.persona_id
        && !sqlx::query_scalar::<_, bool>("select exists(select 1 from personas where id = $1 and company_id = $2)")
            .bind(persona)
            .bind(company)
            .fetch_one(&mut *tx)
            .await?
    {
        return Err(DomainError::NotFound("persona"));
    }
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
    .bind(company)
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
    let companies = read_scope(hub, actor)?;
    if due_within_days.is_some_and(|days| days <= 0) {
        return Err(invalid("invalid_due_within_days", "due_within_days must be a positive number of days"));
    }
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "{SELECT}
         where ($1::text is null or o.state = $1)
           and ($2::int is null or o.deadline <= now() + $2::int * interval '1 day')
           and ($3::text[] is null or o.company_id = any($3))
         order by o.deadline nulls last, o.created_at desc limit 200) t"
    ))
    .bind(state.map(OpportunityState::as_str))
    .bind(due_within_days)
    .bind(companies)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

pub async fn get(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<Opportunity> {
    let companies = read_scope(hub, actor)?;
    let row: Option<Value> = sqlx::query_scalar(&format!("{SELECT} where o.id = $1 and ($2::text[] is null or o.company_id = any($2))) t"))
        .bind(id)
        .bind(companies)
        .fetch_optional(&hub.db)
        .await?;
    decode_one(row.ok_or(DomainError::NotFound("opportunity"))?)
}

/// Every decision lands in the audit log; ingest can never move the state.
pub async fn decide(hub: &Hub, actor: &Actor, id: Uuid, input: OpportunityDecisionIn) -> DResult<Opportunity> {
    let companies = scope(hub, actor, OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let current: Option<String> =
        sqlx::query_scalar("select state from opportunities where id = $1 and ($2::text[] is null or company_id = any($2)) for update")
            .bind(id)
            .bind(companies)
            .fetch_optional(&mut *tx)
            .await?;
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

pub(crate) async fn today(hub: &Hub, companies: Option<Vec<&str>>) -> DResult<Vec<TodayItem>> {
    decode(
        sqlx::query_scalar(
            "select to_jsonb(t) from (
                select 'opportunity' as kind, o.title, 'opportunity' as subject_type, o.id as subject_id,
                    case when o.deadline < now() then 'expired' else o.state end as status, o.deadline as at
                from opportunities o
                where o.state in ('shortlisted', 'drafting') and o.deadline < now() + interval '14 days'
                    and ($1::text[] is null or o.company_id = any($1))
                order by o.deadline limit 50
            ) t",
        )
        .bind(companies)
        .fetch_all(&hub.db)
        .await?,
    )
}
