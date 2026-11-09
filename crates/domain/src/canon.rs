//! Personas and append-only canon revisions. Chat, news and agents may only
//! propose; an operator decision is the only path to `accepted`.

use contracts::{CanonDecisionIn, CanonProposalIn, CanonRevision, CanonSource, Persona, Role};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR, OPERATOR_OR_AGENT, audit, conflict, decode, decode_one, invalid};

const CANON_COLS: &str = "id, persona_id, revision, base_revision, body, state, source, rationale, proposed_by, decided_by, created_at";

pub async fn create_persona(hub: &Hub, actor: &Actor, name: &str) -> DResult<Persona> {
    actor.require(OPERATOR)?;
    let id = Uuid::new_v4();
    let mut tx = hub.db.begin().await?;
    sqlx::query("insert into personas (id, company_id, name) values ($1, $2, $3)")
        .bind(id)
        .bind(&hub.config.company_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, actor, "persona.created", "persona", id, json!({ "name": name })).await?;
    tx.commit().await?;
    get_persona(hub, actor, id).await
}

const PERSONA_SELECT: &str = "select to_jsonb(t) from (
    select p.id, p.company_id, p.name,
      (select to_jsonb(c) from (select id, persona_id, revision, base_revision, body, state, source, rationale, proposed_by, decided_by, created_at
         from canon_revisions where persona_id = p.id and state = 'accepted') c) as canon,
      (select count(*) from canon_revisions where persona_id = p.id and state = 'proposed') as pending_proposals
    from personas p";

pub async fn list_personas(hub: &Hub, actor: &Actor) -> DResult<Vec<Persona>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(&format!("{PERSONA_SELECT} order by p.name) t")).fetch_all(&hub.db).await?;
    decode(rows)
}

pub async fn get_persona(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<Persona> {
    actor.require(crate::ANY_READER)?;
    let row: Option<Value> = sqlx::query_scalar(&format!("{PERSONA_SELECT} where p.id = $1) t")).bind(id).fetch_optional(&hub.db).await?;
    decode_one(row.ok_or(DomainError::NotFound("persona"))?)
}

pub async fn list(hub: &Hub, actor: &Actor, persona_id: Uuid) -> DResult<Vec<CanonRevision>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {CANON_COLS} from canon_revisions where persona_id = $1 order by revision desc) t"
    ))
    .bind(persona_id)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

async fn get(conn: &mut sqlx::PgConnection, id: Uuid) -> DResult<CanonRevision> {
    let row: Option<Value> =
        sqlx::query_scalar(&format!("select to_jsonb(t) from (select {CANON_COLS} from canon_revisions where id = $1) t"))
            .bind(id)
            .fetch_optional(conn)
            .await?;
    decode_one(row.ok_or(DomainError::NotFound("canon revision"))?)
}

/// Record a proposal. Never changes accepted canon.
pub async fn propose(hub: &Hub, actor: &Actor, persona_id: Uuid, input: CanonProposalIn) -> DResult<CanonRevision> {
    actor.require(OPERATOR_OR_AGENT)?;
    // Agents cannot launder a suggestion as the operator's own.
    let source = match (actor.principal.role, input.source) {
        (Role::Agent, CanonSource::Operator) => CanonSource::Agent,
        (_, s) => s,
    };
    if input.body.voice.trim().is_empty() {
        return Err(invalid("invalid_canon", "voice is required"));
    }
    let mut tx = hub.db.begin().await?;
    // Serialize revision numbering per persona.
    sqlx::query("select id from personas where id = $1 for update")
        .bind(persona_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("persona"))?;
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into canon_revisions (id, persona_id, revision, base_revision, body, state, source, rationale, proposed_by)
         select $1, $2, coalesce(max(revision), 0) + 1, $3, $4, 'proposed', $5, $6, $7 from canon_revisions where persona_id = $2",
    )
    .bind(id)
    .bind(persona_id)
    .bind(input.base_revision)
    .bind(serde_json::to_value(&input.body).unwrap_or_default())
    .bind(source.as_str())
    .bind(&input.rationale)
    .bind(actor.name())
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "canon.proposed", "persona", persona_id, json!({ "revision_id": id, "source": source })).await?;
    let rev = get(&mut tx, id).await?;
    tx.commit().await?;
    Ok(rev)
}

pub async fn decide(hub: &Hub, actor: &Actor, revision_id: Uuid, decision: CanonDecisionIn) -> DResult<CanonRevision> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let proposal = get(&mut tx, revision_id).await?;
    sqlx::query("select id from personas where id = $1 for update").bind(proposal.persona_id).execute(&mut *tx).await?;
    // Re-read under the persona lock.
    let proposal = get(&mut tx, revision_id).await?;
    if proposal.state != contracts::CanonState::Proposed {
        return Err(conflict("canon_already_decided", format!("revision is {}", proposal.state)));
    }
    let current: Option<i32> = sqlx::query_scalar("select revision from canon_revisions where persona_id = $1 and state = 'accepted'")
        .bind(proposal.persona_id)
        .fetch_optional(&mut *tx)
        .await?;

    let accept_body = match &decision {
        CanonDecisionIn::Reject { reason } => {
            sqlx::query("update canon_revisions set state = 'rejected', decided_by = $2, decided_at = now(), rationale = coalesce($3, rationale) where id = $1")
                .bind(revision_id)
                .bind(actor.name())
                .bind(reason)
                .execute(&mut *tx)
                .await?;
            audit(
                &mut tx,
                actor,
                "canon.rejected",
                "persona",
                proposal.persona_id,
                json!({ "revision_id": revision_id, "reason": reason }),
            )
            .await?;
            let rev = get(&mut tx, revision_id).await?;
            tx.commit().await?;
            return Ok(rev);
        }
        CanonDecisionIn::Accept => None,
        CanonDecisionIn::AcceptEdited { body } => Some(body.clone()),
    };
    if proposal.base_revision != current {
        return Err(conflict(
            "stale_canon_base",
            format!("proposal was written against revision {:?}; accepted canon is {:?}", proposal.base_revision, current),
        ));
    }
    sqlx::query("update canon_revisions set state = 'superseded' where persona_id = $1 and state = 'accepted'")
        .bind(proposal.persona_id)
        .execute(&mut *tx)
        .await?;
    let accepted_id = match accept_body {
        None => {
            sqlx::query("update canon_revisions set state = 'accepted', decided_by = $2, decided_at = now() where id = $1")
                .bind(revision_id)
                .bind(actor.name())
                .execute(&mut *tx)
                .await?;
            revision_id
        }
        Some(body) => {
            // Edited acceptance appends a new operator revision; the proposal is kept as superseded.
            sqlx::query("update canon_revisions set state = 'superseded', decided_by = $2, decided_at = now() where id = $1")
                .bind(revision_id)
                .bind(actor.name())
                .execute(&mut *tx)
                .await?;
            let id = Uuid::new_v4();
            sqlx::query(
                "insert into canon_revisions (id, persona_id, revision, base_revision, body, state, source, rationale, proposed_by, decided_by, decided_at)
                 select $1, $2, max(revision) + 1, $3, $4, 'accepted', 'operator', $5, $6, $6, now() from canon_revisions where persona_id = $2",
            )
            .bind(id)
            .bind(proposal.persona_id)
            .bind(current)
            .bind(serde_json::to_value(&body).unwrap_or_default())
            .bind(format!("edited from proposal r{}", proposal.revision))
            .bind(actor.name())
            .execute(&mut *tx)
            .await?;
            id
        }
    };
    audit(
        &mut tx,
        actor,
        "canon.accepted",
        "persona",
        proposal.persona_id,
        json!({ "revision_id": accepted_id, "from_proposal": revision_id }),
    )
    .await?;
    let rev = get(&mut tx, accepted_id).await?;
    tx.commit().await?;
    Ok(rev)
}
