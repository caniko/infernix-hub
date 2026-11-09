//! Relationships: inbound interactions, reply drafts, review, human takeover
//! and native-platform handoff.
//!
//! Ownership is versioned by `owner_epoch`. Every ownership change bumps it
//! and invalidates drafts written under the old epoch, so a queued agent
//! reply can never be delivered after a human takes over.

use contracts::{
    Direction, DraftDecisionIn, DraftReplyIn, DraftState, FollowUpIn, HandoffOut, IngestReceipt, InteractionIn, Owner, Relationship,
    RelationshipDetail, ReplyDraft, Role,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ports::{PortError, ProviderState, ReplyRequest};
use crate::{Actor, DResult, DomainError, Hub, OPERATOR, OPERATOR_OR_AGENT, audit, conflict, decode, decode_one, invalid, outbox};

const REL_COLS: &str = "id, account_id, external_thread_id, counterpart, owner, owner_user, owner_epoch, follow_up_at, last_inbound_at, content_item_id, permalink";
const DRAFT_COLS: &str =
    "id, relationship_id, in_reply_to, body, author_kind, author, owner_epoch, state, reviewed_by, provider_reply_id, created_at";

pub async fn ingest(hub: &Hub, actor: &Actor, ev: InteractionIn) -> DResult<IngestReceipt> {
    actor.require(crate::news::INGESTERS)?;
    if ev.external_id.trim().is_empty() || ev.external_thread_id.trim().is_empty() {
        return Err(invalid("invalid_interaction", "external_id and external_thread_id are required"));
    }
    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "insert into inbox (id, source, idempotency_key, kind, payload) values ($1, $2, $3, 'interaction', $4)
         on conflict (source, idempotency_key) do update set deliveries = inbox.deliveries + 1",
    )
    .bind(Uuid::new_v4())
    .bind(format!("interaction:{}", ev.account_id))
    .bind(format!("{}:{}", ev.direction, ev.external_id))
    .bind(serde_json::to_value(&ev).unwrap_or_default())
    .execute(&mut *tx)
    .await?;
    sqlx::query("select id from accounts where id = $1")
        .bind(&ev.account_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("account"))?;
    let counterpart = if ev.direction == Direction::Inbound { ev.author.clone() } else { String::new() };
    let rel_id: Uuid = sqlx::query_scalar(
        "insert into relationships (id, account_id, external_thread_id, counterpart, owner, permalink) values ($1, $2, $3, $4, 'agent', $5)
         on conflict (account_id, external_thread_id) do update
           set counterpart = case when relationships.counterpart = '' then excluded.counterpart else relationships.counterpart end,
               permalink = coalesce(relationships.permalink, excluded.permalink)
         returning id",
    )
    .bind(Uuid::new_v4())
    .bind(&ev.account_id)
    .bind(&ev.external_thread_id)
    .bind(&counterpart)
    .bind(&ev.permalink)
    .fetch_one(&mut *tx)
    .await?;
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "insert into interactions (id, relationship_id, direction, external_id, author, body, occurred_at, raw) values ($1, $2, $3, $4, $5, $6, $7, $8)
         on conflict (relationship_id, direction, external_id) do nothing returning id",
    )
    .bind(Uuid::new_v4())
    .bind(rel_id)
    .bind(ev.direction.as_str())
    .bind(&ev.external_id)
    .bind(&ev.author)
    .bind(&ev.body)
    .bind(ev.occurred_at)
    .bind(&ev.raw)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_some() {
        if ev.direction == Direction::Inbound {
            sqlx::query("update relationships set last_inbound_at = greatest(coalesce(last_inbound_at, $2), $2) where id = $1")
                .bind(rel_id)
                .bind(ev.occurred_at)
                .execute(&mut *tx)
                .await?;
        }
        audit(
            &mut tx,
            actor,
            "interaction.received",
            "relationship",
            rel_id,
            json!({ "external_id": ev.external_id, "direction": ev.direction }),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(IngestReceipt { id: rel_id, created: inserted.is_some(), revision: 1 })
}

pub async fn list(hub: &Hub, actor: &Actor) -> DResult<Vec<Relationship>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {REL_COLS} from relationships order by last_inbound_at desc nulls last limit 200) t"
    ))
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

async fn get_rel(conn: &mut sqlx::PgConnection, id: Uuid, lock: bool) -> DResult<Relationship> {
    let row: Option<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {REL_COLS} from relationships where id = $1 {}) t",
        if lock { "for update" } else { "" }
    ))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    decode_one(row.ok_or(DomainError::NotFound("relationship"))?)
}

async fn get_draft(conn: &mut sqlx::PgConnection, id: Uuid) -> DResult<ReplyDraft> {
    let row: Option<Value> =
        sqlx::query_scalar(&format!("select to_jsonb(t) from (select {DRAFT_COLS} from reply_drafts where id = $1) t"))
            .bind(id)
            .fetch_optional(conn)
            .await?;
    decode_one(row.ok_or(DomainError::NotFound("reply draft"))?)
}

pub async fn detail(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<RelationshipDetail> {
    actor.require(crate::ANY_READER)?;
    let mut conn = hub.db.acquire().await?;
    let relationship = get_rel(&mut conn, id, false).await?;
    let interactions: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(t) from (select id, direction, external_id, author, body, occurred_at from interactions where relationship_id = $1 order by occurred_at, id) t",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    let drafts: Vec<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {DRAFT_COLS} from reply_drafts where relationship_id = $1 order by created_at desc) t"
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);
    Ok(RelationshipDetail {
        relationship,
        interactions: decode(interactions)?,
        drafts: decode(drafts)?,
        history: crate::history(&hub.db, "relationship", &id.to_string()).await?,
    })
}

fn author_kind(actor: &Actor) -> Owner {
    if actor.principal.role == Role::Agent { Owner::Agent } else { Owner::Human }
}

pub async fn draft_reply(hub: &Hub, actor: &Actor, rel_id: Uuid, input: DraftReplyIn) -> DResult<ReplyDraft> {
    actor.require(OPERATOR_OR_AGENT)?;
    if input.body.trim().is_empty() {
        return Err(invalid("invalid_draft", "reply is empty"));
    }
    let mut tx = hub.db.begin().await?;
    let rel = get_rel(&mut tx, rel_id, true).await?;
    let kind = author_kind(actor);
    if input.owner_epoch != rel.owner_epoch {
        return Err(conflict("stale_owner_epoch", format!("ownership changed (epoch {} → {})", input.owner_epoch, rel.owner_epoch)));
    }
    if kind == Owner::Agent && rel.owner == Owner::Human {
        return Err(conflict("human_owns_conversation", "a human has taken over this conversation"));
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into reply_drafts (id, relationship_id, in_reply_to, body, author_kind, author, owner_epoch, state) values ($1, $2, $3, $4, $5, $6, $7, 'in_review')",
    )
    .bind(id)
    .bind(rel_id)
    .bind(input.in_reply_to)
    .bind(&input.body)
    .bind(kind.as_str())
    .bind(actor.name())
    .bind(rel.owner_epoch)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "reply.drafted", "relationship", rel_id, json!({ "draft_id": id, "owner_epoch": rel.owner_epoch })).await?;
    let d = get_draft(&mut tx, id).await?;
    tx.commit().await?;
    Ok(d)
}

/// A draft is deliverable only under the epoch it was written in, and agent
/// drafts only while the agent still owns the conversation.
pub fn draft_current(d: &ReplyDraft, rel: &Relationship) -> bool {
    d.owner_epoch == rel.owner_epoch && !(d.author_kind == Owner::Agent && rel.owner == Owner::Human)
}

pub async fn decide_draft(hub: &Hub, actor: &Actor, draft_id: Uuid, decision: DraftDecisionIn) -> DResult<ReplyDraft> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let d = get_draft(&mut tx, draft_id).await?;
    let rel = get_rel(&mut tx, d.relationship_id, true).await?;
    let d = get_draft(&mut tx, draft_id).await?;
    if d.state != DraftState::InReview {
        return Err(conflict("draft_not_in_review", format!("draft is {}", d.state)));
    }
    match decision {
        DraftDecisionIn::Reject { reason } => {
            sqlx::query("update reply_drafts set state = 'rejected', reviewed_by = $2, updated_at = now() where id = $1")
                .bind(draft_id)
                .bind(actor.name())
                .execute(&mut *tx)
                .await?;
            audit(&mut tx, actor, "reply.rejected", "relationship", rel.id, json!({ "draft_id": draft_id, "reason": reason })).await?;
        }
        DraftDecisionIn::Approve => {
            if !draft_current(&d, &rel) {
                sqlx::query("update reply_drafts set state = 'invalidated', updated_at = now() where id = $1")
                    .bind(draft_id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Err(conflict("stale_owner_epoch", "ownership changed since this draft was written"));
            }
            let caps: Value =
                sqlx::query_scalar("select capabilities from accounts where id = $1").bind(&rel.account_id).fetch_one(&mut *tx).await?;
            if caps["replies"] != json!(true) {
                return Err(conflict("replies_unsupported", "this account cannot reply via API; use Open in platform"));
            }
            sqlx::query("update reply_drafts set state = 'queued', reviewed_by = $2, updated_at = now() where id = $1")
                .bind(draft_id)
                .bind(actor.name())
                .execute(&mut *tx)
                .await?;
            outbox::enqueue(&mut tx, actor, outbox::PUBLISHER_SEND_REPLY, &format!("reply:{draft_id}"), json!({ "draft_id": draft_id }))
                .await?;
            audit(&mut tx, actor, "reply.approved", "relationship", rel.id, json!({ "draft_id": draft_id })).await?;
        }
    }
    let d = get_draft(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(d)
}

async fn set_owner(hub: &Hub, actor: &Actor, rel_id: Uuid, owner: Owner) -> DResult<Relationship> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let rel = get_rel(&mut tx, rel_id, true).await?;
    let user = (owner == Owner::Human).then(|| actor.name());
    sqlx::query("update relationships set owner = $2, owner_user = $3, owner_epoch = owner_epoch + 1 where id = $1")
        .bind(rel_id)
        .bind(owner.as_str())
        .bind(&user)
        .execute(&mut *tx)
        .await?;
    // Outbox rows for these drafts stay; delivery re-checks the draft state.
    let invalidated: Vec<Uuid> = sqlx::query_scalar(
        "update reply_drafts set state = 'invalidated', updated_at = now()
         where relationship_id = $1 and owner_epoch = $2 and state in ('draft', 'in_review', 'approved', 'queued') returning id",
    )
    .bind(rel_id)
    .bind(rel.owner_epoch)
    .fetch_all(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        if owner == Owner::Human { "relationship.takeover" } else { "relationship.released" },
        "relationship",
        rel_id,
        json!({ "epoch": rel.owner_epoch + 1, "invalidated": invalidated }),
    )
    .await?;
    let rel = get_rel(&mut tx, rel_id, false).await?;
    tx.commit().await?;
    Ok(rel)
}

pub async fn takeover(hub: &Hub, actor: &Actor, rel_id: Uuid) -> DResult<Relationship> {
    set_owner(hub, actor, rel_id, Owner::Human).await
}

pub async fn release_to_agent(hub: &Hub, actor: &Actor, rel_id: Uuid) -> DResult<Relationship> {
    set_owner(hub, actor, rel_id, Owner::Agent).await
}

/// Explicit handoff to the native platform: the human takes ownership (so no
/// hub reply can race the manual one) and gets the thread URL.
pub async fn handoff(hub: &Hub, actor: &Actor, rel_id: Uuid) -> DResult<HandoffOut> {
    actor.require(OPERATOR)?;
    let mut conn = hub.db.acquire().await?;
    let rel = get_rel(&mut conn, rel_id, false).await?;
    drop(conn);
    let url = rel.permalink.clone().ok_or_else(|| conflict("no_permalink", "no platform link is known for this thread"))?;
    if rel.owner != Owner::Human {
        takeover(hub, actor, rel_id).await?;
    }
    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "update reply_drafts set state = 'handed_off', updated_at = now() where relationship_id = $1 and state in ('draft', 'in_review')",
    )
    .bind(rel_id)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "relationship.handoff", "relationship", rel_id, json!({ "url": url })).await?;
    tx.commit().await?;
    Ok(HandoffOut { url })
}

pub async fn set_follow_up(hub: &Hub, actor: &Actor, rel_id: Uuid, input: FollowUpIn) -> DResult<Relationship> {
    actor.require(OPERATOR_OR_AGENT)?;
    let mut tx = hub.db.begin().await?;
    get_rel(&mut tx, rel_id, true).await?;
    sqlx::query("update relationships set follow_up_at = $2 where id = $1").bind(rel_id).bind(input.follow_up_at).execute(&mut *tx).await?;
    audit(&mut tx, actor, "relationship.follow_up", "relationship", rel_id, json!({ "follow_up_at": input.follow_up_at })).await?;
    let rel = get_rel(&mut tx, rel_id, false).await?;
    tx.commit().await?;
    Ok(rel)
}

/// Outcome of a delivery attempt, for the worker's outbox bookkeeping.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplyDelivery {
    Sent,
    /// Draft no longer current (takeover, rejection…); nothing was sent.
    Skipped(String),
    Unknown,
    /// Not sent; retry the outbox delivery.
    Retry(String),
    Failed(String),
}

/// Worker: deliver one approved reply, re-checking currency under the lock.
pub async fn deliver_reply(hub: &Hub, draft_id: Uuid) -> DResult<ReplyDelivery> {
    let mut tx = hub.db.begin().await?;
    let d = get_draft(&mut tx, draft_id).await?;
    let rel = get_rel(&mut tx, d.relationship_id, true).await?;
    let d = get_draft(&mut tx, draft_id).await?;
    if d.state != DraftState::Queued {
        return Ok(ReplyDelivery::Skipped(format!("draft is {}", d.state)));
    }
    if !draft_current(&d, &rel) {
        sqlx::query("update reply_drafts set state = 'invalidated', updated_at = now() where id = $1")
            .bind(draft_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(ReplyDelivery::Skipped("ownership changed".into()));
    }
    let in_reply_to: Option<String> = match d.in_reply_to {
        Some(i) => sqlx::query_scalar("select external_id from interactions where id = $1").bind(i).fetch_optional(&mut *tx).await?,
        None => None,
    };
    sqlx::query("update reply_drafts set state = 'sending', updated_at = now() where id = $1").bind(draft_id).execute(&mut *tx).await?;
    tx.commit().await?;

    let req = ReplyRequest {
        account_id: rel.account_id.clone(),
        idempotency_key: format!("ihub-reply-{draft_id}"),
        thread_id: rel.external_thread_id.clone(),
        in_reply_to,
        text: d.body.clone(),
    };
    let result = hub.publisher.reply(&req).await;
    let mut tx = hub.db.begin().await?;
    let out = match result {
        Ok(post) if post.state != ProviderState::Failed => {
            sqlx::query("update reply_drafts set state = 'sent', provider_reply_id = $2, updated_at = now() where id = $1")
                .bind(draft_id)
                .bind(&post.provider_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "insert into interactions (id, relationship_id, direction, external_id, author, body, occurred_at, raw)
                 values ($1, $2, 'outbound', $3, $4, $5, now(), $6) on conflict do nothing",
            )
            .bind(Uuid::new_v4())
            .bind(rel.id)
            .bind(&post.provider_id)
            .bind(&d.author)
            .bind(&d.body)
            .bind(&post.raw)
            .execute(&mut *tx)
            .await?;
            ReplyDelivery::Sent
        }
        Ok(post) => {
            sqlx::query("update reply_drafts set state = 'rejected', updated_at = now() where id = $1")
                .bind(draft_id)
                .execute(&mut *tx)
                .await?;
            ReplyDelivery::Failed(post.raw.to_string())
        }
        Err(PortError::Ambiguous(e)) => {
            sqlx::query("update reply_drafts set state = 'outcome_unknown', updated_at = now() where id = $1")
                .bind(draft_id)
                .execute(&mut *tx)
                .await?;
            let _ = e;
            ReplyDelivery::Unknown
        }
        Err(PortError::Unreachable(e)) => {
            sqlx::query("update reply_drafts set state = 'queued', updated_at = now() where id = $1")
                .bind(draft_id)
                .execute(&mut *tx)
                .await?;
            ReplyDelivery::Retry(e)
        }
        Err(e) => {
            sqlx::query("update reply_drafts set state = 'rejected', updated_at = now() where id = $1")
                .bind(draft_id)
                .execute(&mut *tx)
                .await?;
            ReplyDelivery::Failed(e.to_string())
        }
    };
    audit(
        &mut tx,
        &Actor::system(),
        "reply.delivery",
        "relationship",
        rel.id,
        json!({ "draft_id": draft_id, "result": format!("{out:?}") }),
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}

/// Worker: confirm `outcome_unknown` replies before anything else happens.
pub async fn reconcile_replies(hub: &Hub) -> DResult<()> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "select d.id, r.account_id from reply_drafts d join relationships r on r.id = d.relationship_id
         where d.state = 'outcome_unknown' or (d.state = 'sending' and d.updated_at < now() - interval '2 minutes')",
    )
    .fetch_all(&hub.db)
    .await?;
    for (id, account) in rows {
        match hub.publisher.lookup_reply(&account, &format!("ihub-reply-{id}")).await {
            Ok(Some(post)) => {
                sqlx::query("update reply_drafts set state = 'sent', provider_reply_id = $2, updated_at = now() where id = $1")
                    .bind(id)
                    .bind(&post.provider_id)
                    .execute(&hub.db)
                    .await?;
            }
            // Confirmed absent: safe to surface as not sent (operator may redraft).
            Ok(None) => {
                sqlx::query("update reply_drafts set state = 'rejected', updated_at = now() where id = $1")
                    .bind(id)
                    .execute(&hub.db)
                    .await?;
            }
            Err(_) => {
                sqlx::query("update reply_drafts set state = 'outcome_unknown', updated_at = now() where id = $1 and state = 'sending'")
                    .bind(id)
                    .execute(&hub.db)
                    .await?;
            }
        }
    }
    Ok(())
}
