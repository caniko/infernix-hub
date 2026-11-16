//! Personas and append-only canon revisions. Chat, news and agents may only
//! propose; an operator decision is the only path to `accepted`.

use contracts::{
    CanonBody, CanonCommentIn, CanonDecisionIn, CanonDraftIn, CanonProposalIn, CanonRevision, CanonSource, CanonState, CharacterProfile,
    Persona, Role, Severity,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR, OPERATOR_OR_AGENT, audit, conflict, decode, decode_one, invalid};

const CANON_COLS: &str = "id, persona_id, revision, base_revision, body, state, source, rationale, proposed_by, decided_by, created_at, review_requested_at, review_issue_url, (select coalesce(jsonb_agg(to_jsonb(cc) order by cc.created_at, cc.id), '[]'::jsonb) from canon_comments cc where cc.revision_id = canon_revisions.id) as comments";

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
    select p.id, p.company_id, p.name, p.profile,
      (select asset_id from persona_references where persona_id=p.id order by pinned_at desc, id desc limit 1) as reference_asset_id,
      (select to_jsonb(c) from (select id, persona_id, revision, base_revision, body, state, source, rationale, proposed_by, decided_by, created_at, review_requested_at, review_issue_url,
          (select coalesce(jsonb_agg(to_jsonb(cc) order by cc.created_at, cc.id), '[]'::jsonb) from canon_comments cc where cc.revision_id = canon_revisions.id) as comments
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

pub async fn set_profile(hub: &Hub, actor: &Actor, persona_id: Uuid, profile: CharacterProfile) -> DResult<Persona> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let changed = sqlx::query("update personas set profile = $2 where id = $1")
        .bind(persona_id)
        .bind(serde_json::to_value(&profile).map_err(|e| DomainError::Internal(e.to_string()))?)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(DomainError::NotFound("persona"));
    }
    audit(
        &mut tx,
        actor,
        "persona.profile_set",
        "persona",
        persona_id,
        json!({"fields": ["bio", "reads_as", "look", "palette", "voice", "canon_draft"]}),
    )
    .await?;
    tx.commit().await?;
    get_persona(hub, actor, persona_id).await
}

pub async fn draft_canon(hub: &Hub, actor: &Actor, persona_id: Uuid, input: CanonDraftIn) -> DResult<CanonBody> {
    actor.require(OPERATOR)?;
    let persona = get_persona(hub, actor, persona_id).await?;
    let context = json!({"task": "draft_canon", "persona": {"name": persona.name, "canon": persona.canon}, "bio": input.bio});
    let turns = [crate::ports::AssistantTurn { role: "operator".into(), body: input.bio }];
    hub.assistant
        .reply(&context, &turns)
        .await
        .map_err(|e| DomainError::Unavailable { code: "assistant_unavailable", message: e.to_string() })?
        .canon_suggestion
        .ok_or_else(|| DomainError::Unavailable { code: "no_canon_draft", message: "assistant returned no canon draft".into() })
}

pub async fn request_comments(hub: &Hub, actor: &Actor, revision_id: Uuid) -> DResult<CanonRevision> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let proposal = get(&mut tx, revision_id).await?;
    sqlx::query("select id from personas where id = $1 for update").bind(proposal.persona_id).execute(&mut *tx).await?;
    let proposal = get(&mut tx, revision_id).await?;
    if proposal.state != CanonState::Proposed {
        return Err(conflict("canon_already_decided", "revision is not proposed"));
    }
    if proposal.review_requested_at.is_some() {
        tx.commit().await?;
        return Ok(proposal);
    }
    let studio: Option<Value> =
        sqlx::query_scalar("select value from settings where key = 'paperclip_studio'").fetch_optional(&mut *tx).await?;
    let field = |key: &str| studio.as_ref().and_then(|v| v[key].as_str()).map(str::to_string);
    let ceo = field("ceo_agent_id")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid("ceo_not_configured", "CEO agent is not configured"))?;
    let (name, profile): (String, Value) =
        sqlx::query_as("select name, profile from personas where id = $1").bind(proposal.persona_id).fetch_one(&mut *tx).await?;
    let description = format!(
        "Voice: {}\nRules:\n{}\nAvoid:\n{}\nWorld:\n{}\nbase_revision: {:?}\nBio:\n{}\n\nComment with the hub MCP tool hub_canon_comment (revision_id {revision_id}). Do not edit canon. The operator accepts or rejects.",
        proposal.body.voice,
        proposal.body.rules.join("\n"),
        proposal.body.avoid.join("\n"),
        proposal.body.world.join("\n"),
        proposal.base_revision,
        profile["bio"].as_str().unwrap_or_default()
    );
    sqlx::query("update canon_revisions set review_requested_at = now() where id = $1").bind(revision_id).execute(&mut *tx).await?;
    crate::outbox::enqueue(&mut tx, actor, crate::outbox::PAPERCLIP_CREATE_ISSUE, &format!("issue:canon:{revision_id}"), json!({
        "canon_revision_id": revision_id, "company_id": hub.config.company_id, "title": format!("Canon r{} for {name}: comments wanted", proposal.revision),
        "description": description, "origin_key": format!("fleetix-hub:canon:{revision_id}"), "project_id": field("project_id"), "assignee_agent_id": ceo
    })).await?;
    audit(&mut tx, actor, "canon.comments_requested", "persona", proposal.persona_id, json!({"revision_id": revision_id})).await?;
    let proposal = get(&mut tx, revision_id).await?;
    tx.commit().await?;
    Ok(proposal)
}

pub async fn comment(hub: &Hub, actor: &Actor, revision_id: Uuid, input: CanonCommentIn) -> DResult<CanonRevision> {
    actor.require(OPERATOR_OR_AGENT)?;
    if input.body.trim().is_empty() || input.body.chars().count() > 16384 {
        return Err(invalid("invalid_comment", "comment must contain 1 to 16384 characters"));
    }
    let mut tx = hub.db.begin().await?;
    let proposal = get(&mut tx, revision_id).await?;
    sqlx::query("select id from personas where id = $1 for update").bind(proposal.persona_id).execute(&mut *tx).await?;
    let proposal = get(&mut tx, revision_id).await?;
    if proposal.state != CanonState::Proposed {
        return Err(conflict("canon_already_decided", "revision is not proposed"));
    }
    let comment_id = Uuid::new_v4();
    sqlx::query("insert into canon_comments (id, revision_id, author, author_role, body) values ($1, $2, $3, $4, $5)")
        .bind(comment_id)
        .bind(revision_id)
        .bind(actor.name())
        .bind(actor.principal.role.to_string())
        .bind(&input.body)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, actor, "canon.commented", "persona", proposal.persona_id, json!({"revision_id": revision_id})).await?;
    let name: String = sqlx::query_scalar("select name from personas where id = $1").bind(proposal.persona_id).fetch_one(&mut *tx).await?;
    let proposal = get(&mut tx, revision_id).await?;
    tx.commit().await?;
    if actor.principal.role == Role::Agent {
        let raised = crate::notifications::raise(
            hub,
            &crate::notifications::Raise {
                source: "canon".into(),
                dedupe_key: format!("canon:comment:{revision_id}"),
                severity: Severity::Info,
                title: format!("{name}: comment on canon r{}", proposal.revision),
                // A new body per comment re-opens the row after the operator acknowledged an earlier one.
                body: format!("{}\n(comment {comment_id})", input.body.chars().take(280).collect::<String>()),
                subject_type: Some("persona".into()),
                subject_id: Some(proposal.persona_id.to_string()),
                url: None,
            },
        )
        .await;
        // The comment is committed; a retry after a notification error would duplicate it.
        if let Err(e) = raised {
            tracing::warn!(%revision_id, error = %e, "canon comment saved but its notification failed");
        }
    }
    Ok(proposal)
}

pub async fn reviews(hub: &Hub, actor: &Actor) -> DResult<Vec<CanonRevision>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(&format!("select to_jsonb(t) from (select {CANON_COLS} from canon_revisions where state = 'proposed' and review_requested_at is not null order by review_requested_at, id) t")).fetch_all(&hub.db).await?;
    decode(rows)
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
