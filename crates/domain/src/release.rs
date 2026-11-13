//! Frozen release revisions, native approval binding, scheduling and the
//! guarded publisher dispatch.
//!
//! Invariants:
//! - A release revision's actions are immutable; any edit is a new revision.
//! - An approval authorizes exactly one revision's `payload_hash`.
//! - One intent per (revision, account); duplicate timers converge on it.
//! - A timeout after submission is `outcome_unknown`, never a resubmit.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use contracts::{
    ContentState, DispatchIn, DispatchOut, FreezeReleaseIn, IntentState, LinkApprovalIn, MediaRef, PublicationIntent, ReleaseAction,
    ReleaseRevision, ReleaseState, ResolveIntentIn, Role, ScheduleIn,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ports::{ApprovalStatus, ApprovalView, MediaBlob, PortError, ProviderState, PublishRequest, Publisher};
use crate::{Actor, DResult, DomainError, Hub, OPERATOR, audit, conflict, decode, decode_one, hash, invalid, outbox};

const RELEASE_COLS: &str =
    "id, content_item_id, revision, actions, payload_hash, state, paperclip_approval_id, approved_by, approved_at, created_at";

async fn get_release(conn: &mut sqlx::PgConnection, id: Uuid, lock: bool) -> DResult<ReleaseRevision> {
    let sql = format!(
        "select to_jsonb(t) from (select {RELEASE_COLS} from release_revisions where id = $1 {}) t",
        if lock { "for update" } else { "" }
    );
    let row: Option<Value> = sqlx::query_scalar(&sql).bind(id).fetch_optional(conn).await?;
    decode_one(row.ok_or(DomainError::NotFound("release revision"))?)
}

pub async fn list_for_content(hub: &Hub, content_id: Uuid) -> DResult<Vec<ReleaseRevision>> {
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {RELEASE_COLS} from release_revisions where content_item_id = $1 order by revision desc) t"
    ))
    .bind(content_id)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

/// Integrity check: the stored actions still hash to the stored hash.
fn verify_payload(rel: &ReleaseRevision) -> DResult<()> {
    if hash::release_hash(&rel.actions) != rel.payload_hash {
        return Err(DomainError::Internal(format!("release {} payload no longer matches its hash", rel.id)));
    }
    Ok(())
}

/// Freeze the complete action per destination account into a new revision.
pub async fn freeze(hub: &Hub, actor: &Actor, content_id: Uuid, input: FreezeReleaseIn) -> DResult<ReleaseRevision> {
    actor.require(OPERATOR)?;
    if input.account_ids.is_empty() {
        return Err(invalid("invalid_release", "at least one account is required"));
    }
    if input.timing.not_before >= input.timing.not_after {
        return Err(invalid("invalid_timing", "not_before must be before not_after"));
    }
    if input.text.trim().is_empty() {
        return Err(invalid("invalid_release", "text is required"));
    }
    let mut tx = hub.db.begin().await?;
    sqlx::query("select id from content_items where id = $1 for update")
        .bind(content_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("content item"))?;
    let media: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "select id, sha256, mime, alt_text from assets where content_item_id = $1 and selected and state = 'ready' order by created_at, id",
    )
    .bind(content_id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some((id, ..)) = media.iter().find(|(.., alt)| alt.trim().is_empty()) {
        return Err(invalid("missing_alt_text", format!("selected asset {id} has no alt text")));
    }
    let media: Vec<MediaRef> =
        media.into_iter().map(|(asset_id, sha256, mime, alt_text)| MediaRef { asset_id, sha256, mime, alt_text }).collect();
    let mut actions = vec![];
    let mut account_ids = input.account_ids.clone();
    account_ids.sort();
    account_ids.dedup();
    for account in &account_ids {
        let row: Option<(String, String, Value)> = sqlx::query_as("select company_id, transport, capabilities from accounts where id = $1")
            .bind(account)
            .fetch_optional(&mut *tx)
            .await?;
        let (company_id, transport, caps) = row.ok_or(DomainError::NotFound("account"))?;
        if caps["publish"] != json!(true) {
            return Err(invalid("account_cannot_publish", format!("account {account} has no publish capability")));
        }
        actions.push(ReleaseAction {
            company_id,
            account_id: account.clone(),
            // Frozen into the payload so dispatch never depends on a later
            // change to the account's transport.
            transport: Some(transport),
            text: input.text.clone(),
            media: media.clone(),
            platform_settings: input.platform_settings.clone(),
            reply_to: input.reply_to.clone(),
            timing: input.timing.clone(),
        });
    }
    let payload_hash = hash::release_hash(&actions);
    // Older unscheduled revisions are superseded; their unclaimed intents
    // are cancelled so they can never dispatch.
    let superseded: Vec<Uuid> = sqlx::query_scalar(
        "update release_revisions set state = 'superseded' where content_item_id = $1 and state in ('draft', 'in_review', 'approved') returning id",
    )
    .bind(content_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("update publication_intents set state = 'cancelled', updated_at = now() where release_revision_id = any($1) and state = 'scheduled'")
        .bind(&superseded)
        .execute(&mut *tx)
        .await?;
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into release_revisions (id, content_item_id, revision, actions, payload_hash, state, created_by)
         select $1, $2, coalesce(max(revision), 0) + 1, $3, $4, 'draft', $5 from release_revisions where content_item_id = $2",
    )
    .bind(id)
    .bind(content_id)
    .bind(serde_json::to_value(&actions).unwrap_or_default())
    .bind(&payload_hash)
    .bind(actor.name())
    .execute(&mut *tx)
    .await?;
    crate::content::set_state(&mut tx, content_id, ContentState::Review).await?;
    audit(
        &mut tx,
        actor,
        "release.frozen",
        "content",
        content_id,
        json!({ "release_revision_id": id, "payload_hash": payload_hash, "superseded": superseded }),
    )
    .await?;
    let rel = get_release(&mut tx, id, false).await?;
    tx.commit().await?;
    Ok(rel)
}

/// Ask Paperclip for a native approval of this exact revision.
pub async fn request_review(hub: &Hub, actor: &Actor, release_id: Uuid) -> DResult<ReleaseRevision> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let rel = get_release(&mut tx, release_id, true).await?;
    if rel.state != ReleaseState::Draft {
        return Err(conflict("release_not_draft", format!("release is {}", rel.state)));
    }
    verify_payload(&rel)?;
    let (title, issue): (String, Option<String>) = sqlx::query_as("select title, paperclip_issue_id from content_items where id = $1")
        .bind(rel.content_item_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("update release_revisions set state = 'in_review' where id = $1").bind(release_id).execute(&mut *tx).await?;
    let accounts: Vec<&str> = rel.actions.iter().map(|a| a.account_id.as_str()).collect();
    outbox::enqueue(
        &mut tx,
        actor,
        outbox::PAPERCLIP_CREATE_APPROVAL,
        &format!("approval:{release_id}"),
        json!({
            "release_revision_id": release_id, "payload_hash": rel.payload_hash, "company_id": hub.config.company_id,
            "title": format!("Publish \"{title}\" r{}", rel.revision),
            "summary": format!("Accounts: {}. Approving authorizes exactly payload {}.", accounts.join(", "), rel.payload_hash),
            "issue_id": issue, "actions": rel.actions,
        }),
    )
    .await?;
    audit(
        &mut tx,
        actor,
        "release.review_requested",
        "content",
        rel.content_item_id,
        json!({ "release_revision_id": release_id, "payload_hash": rel.payload_hash }),
    )
    .await?;
    let rel = get_release(&mut tx, release_id, false).await?;
    tx.commit().await?;
    Ok(rel)
}

/// Check an authoritative approval read against a revision.
pub fn approval_matches(rel: &ReleaseRevision, view: &ApprovalView) -> DResult<()> {
    if view.payload_hash.as_deref() != Some(rel.payload_hash.as_str())
        || view.release_revision_id.as_deref() != Some(rel.id.to_string().as_str())
    {
        return Err(conflict(
            "approval_payload_mismatch",
            format!("approval {} authorizes {:?}, not revision {} ({})", view.id, view.payload_hash, rel.id, rel.payload_hash),
        ));
    }
    Ok(())
}

/// Record the created approval id (worker, after outbox delivery).
pub async fn record_approval_created(hub: &Hub, release_id: Uuid, approval_id: &str) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    sqlx::query("update release_revisions set paperclip_approval_id = $2 where id = $1 and paperclip_approval_id is null")
        .bind(release_id)
        .bind(approval_id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &Actor::system(), "release.approval_created", "release", release_id, json!({ "approval_id": approval_id })).await?;
    tx.commit().await?;
    Ok(())
}

/// Operator links an approval that was created in Paperclip directly. The
/// approval must authorize this exact revision.
pub async fn link_approval(hub: &Hub, actor: &Actor, release_id: Uuid, input: LinkApprovalIn) -> DResult<ReleaseRevision> {
    actor.require(OPERATOR)?;
    let view = hub
        .paperclip
        .get_approval(&input.approval_id)
        .await
        .map_err(|e| DomainError::Unavailable { code: "approval_unverifiable", message: e.to_string() })?;
    let mut tx = hub.db.begin().await?;
    let rel = get_release(&mut tx, release_id, true).await?;
    approval_matches(&rel, &view)?;
    if !matches!(rel.state, ReleaseState::Draft | ReleaseState::InReview) {
        return Err(conflict("release_not_reviewable", format!("release is {}", rel.state)));
    }
    let used: Option<Uuid> = sqlx::query_scalar("select id from release_revisions where paperclip_approval_id = $1 and id <> $2")
        .bind(&input.approval_id)
        .bind(release_id)
        .fetch_optional(&mut *tx)
        .await?;
    if used.is_some() {
        return Err(conflict("approval_already_used", "approval is bound to another revision"));
    }
    sqlx::query("update release_revisions set paperclip_approval_id = $2, state = 'in_review' where id = $1")
        .bind(release_id)
        .bind(&input.approval_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        actor,
        "release.approval_linked",
        "content",
        rel.content_item_id,
        json!({ "release_revision_id": release_id, "approval_id": input.approval_id }),
    )
    .await?;
    tx.commit().await?;
    apply_approval(hub, release_id, &view).await?;
    let mut conn = hub.db.acquire().await?;
    get_release(&mut conn, release_id, false).await
}

/// Apply an authoritative approval read (worker reconcile, notification hint).
pub async fn apply_approval(hub: &Hub, release_id: Uuid, view: &ApprovalView) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    let rel = get_release(&mut tx, release_id, true).await?;
    if rel.state != ReleaseState::InReview || rel.paperclip_approval_id.as_deref() != Some(view.id.as_str()) {
        return Ok(());
    }
    let action = match (approval_matches(&rel, view), view.status) {
        (Err(_), _) => {
            sqlx::query("update release_revisions set state = 'rejected' where id = $1").bind(release_id).execute(&mut *tx).await?;
            "release.approval_mismatch"
        }
        (Ok(()), ApprovalStatus::Approved) => {
            let Some(user) = &view.decided_by_user_id else {
                // No human attribution: do not treat as approved.
                return Ok(());
            };
            sqlx::query(
                "update release_revisions set state = 'approved', approved_by = $2, approved_at = coalesce($3, now()) where id = $1",
            )
            .bind(release_id)
            .bind(format!("paperclip:{user}"))
            .bind(view.decided_at)
            .execute(&mut *tx)
            .await?;
            "release.approved"
        }
        (Ok(()), ApprovalStatus::Rejected | ApprovalStatus::RevisionRequested | ApprovalStatus::Cancelled) => {
            sqlx::query("update release_revisions set state = 'rejected' where id = $1").bind(release_id).execute(&mut *tx).await?;
            "release.rejected"
        }
        (Ok(()), ApprovalStatus::Pending) => return Ok(()),
    };
    audit(
        &mut tx,
        &Actor::system(),
        action,
        "content",
        rel.content_item_id,
        json!({ "release_revision_id": release_id, "approval": view.raw, "decided_by": view.decided_by_user_id }),
    )
    .await?;
    tx.commit().await?;
    // The item leaves review: ready to schedule, or back to selecting for a
    // new revision. A separate transaction after the release lock is gone,
    // taking only the content lock (freeze holds it while adding a revision),
    // then re-reading in a fresh statement that this release is still the
    // latest revision with the decision just written.
    let approved = action == "release.approved";
    let mut tx = hub.db.begin().await?;
    let content_state: String = sqlx::query_scalar("select state from content_items where id = $1 for update")
        .bind(rel.content_item_id)
        .fetch_one(&mut *tx)
        .await?;
    let latest: Option<(Uuid, String)> =
        sqlx::query_as("select id, state from release_revisions where content_item_id = $1 order by revision desc limit 1")
            .bind(rel.content_item_id)
            .fetch_optional(&mut *tx)
            .await?;
    let expected = if approved { "approved" } else { "rejected" };
    if content_state == ContentState::Review.as_str() && latest.as_ref().is_some_and(|(id, st)| *id == release_id && st == expected) {
        crate::content::set_state(&mut tx, rel.content_item_id, if approved { ContentState::Approved } else { ContentState::Selecting })
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn releases_in_review(hub: &Hub) -> DResult<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as(
        "select id, paperclip_approval_id from release_revisions where state = 'in_review' and paperclip_approval_id is not null",
    )
    .fetch_all(&hub.db)
    .await?)
}

// ── Scheduling ──────────────────────────────────────────────────────────────

const INTENT_SELECT: &str = "select to_jsonb(t) from (
    select i.id, i.release_revision_id, i.account_id, i.schedule_revision, i.due_at, i.state, i.provider_post_id, i.provider_url, i.updated_at,
      coalesce((select jsonb_agg(to_jsonb(d) order by d.attempt) from (
        select id, attempt, idempotency_key, state, detail, created_at from delivery_attempts where intent_id = i.id) d), '[]') as attempts
    from publication_intents i";

pub async fn get_intent(conn: &mut sqlx::PgConnection, id: Uuid) -> DResult<PublicationIntent> {
    let row: Option<Value> = sqlx::query_scalar(&format!("{INTENT_SELECT} where i.id = $1) t")).bind(id).fetch_optional(conn).await?;
    decode_one(row.ok_or(DomainError::NotFound("publication intent"))?)
}

pub async fn intents_for_content(hub: &Hub, content_id: Uuid) -> DResult<Vec<PublicationIntent>> {
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "{INTENT_SELECT} join release_revisions r on r.id = i.release_revision_id where r.content_item_id = $1 order by i.due_at) t"
    ))
    .bind(content_id)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

pub async fn all_intents(hub: &Hub, actor: &Actor) -> DResult<Vec<PublicationIntent>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(&format!("{INTENT_SELECT} order by i.due_at desc limit 500) t")).fetch_all(&hub.db).await?;
    decode(rows)
}

/// Schedule (or reschedule within the approved timing scope). Rescheduling
/// bumps `schedule_revision`, so earlier timers are rejected as stale.
pub async fn schedule(hub: &Hub, actor: &Actor, release_id: Uuid, input: ScheduleIn) -> DResult<Vec<PublicationIntent>> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let rel = get_release(&mut tx, release_id, true).await?;
    if rel.state != ReleaseState::Approved {
        return Err(conflict("release_not_approved", format!("release is {}", rel.state)));
    }
    verify_payload(&rel)?;
    let mut ids = vec![];
    for action in &rel.actions {
        if input.due_at < action.timing.not_before || input.due_at > action.timing.not_after {
            return Err(conflict(
                "outside_timing_scope",
                format!(
                    "{} is outside the approved window {}..{}; freeze a new revision for review",
                    input.due_at, action.timing.not_before, action.timing.not_after
                ),
            ));
        }
        let existing: Option<(Uuid, String)> =
            sqlx::query_as("select id, state from publication_intents where release_revision_id = $1 and account_id = $2 for update")
                .bind(release_id)
                .bind(&action.account_id)
                .fetch_optional(&mut *tx)
                .await?;
        let (id, rev): (Uuid, i32) = match existing {
            None => sqlx::query_as(
                "insert into publication_intents (id, release_revision_id, account_id, due_at, state) values ($1, $2, $3, $4, 'scheduled')
                 returning id, schedule_revision",
            )
            .bind(Uuid::new_v4())
            .bind(release_id)
            .bind(&action.account_id)
            .bind(input.due_at)
            .fetch_one(&mut *tx)
            .await?,
            // `failed` is re-armable only once reconciliation proved nothing was published.
            Some((id, state)) if state == "scheduled" || state == "failed" => sqlx::query_as(
                "update publication_intents set due_at = $2, schedule_revision = schedule_revision + 1, state = 'scheduled', updated_at = now()
                 where id = $1 returning id, schedule_revision",
            )
            .bind(id)
            .bind(input.due_at)
            .fetch_one(&mut *tx)
            .await?,
            Some((_, state)) => {
                return Err(conflict("intent_not_reschedulable", format!("{} intent is {state}", action.account_id)));
            }
        };
        let timer = crate::ports::PublicationTimer {
            intent_id: id.to_string(),
            schedule_revision: rev,
            due_at: input.due_at,
            dispatch_url: format!("{}/v1/intents/{id}/dispatch", hub.config.public_url),
        };
        outbox::enqueue(
            &mut tx,
            actor,
            outbox::WINDMILL_SCHEDULE_PUBLICATION,
            &format!("timer:{id}:{rev}"),
            serde_json::to_value(&timer).unwrap_or_default(),
        )
        .await?;
        ids.push(id);
    }
    crate::content::set_state(&mut tx, rel.content_item_id, ContentState::Scheduled).await?;
    audit(
        &mut tx,
        actor,
        "release.scheduled",
        "content",
        rel.content_item_id,
        json!({ "release_revision_id": release_id, "due_at": input.due_at, "intents": ids }),
    )
    .await?;
    let mut out = vec![];
    for id in ids {
        out.push(get_intent(&mut tx, id).await?);
    }
    tx.commit().await?;
    Ok(out)
}

pub async fn cancel_intent(hub: &Hub, actor: &Actor, intent_id: Uuid) -> DResult<PublicationIntent> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let state: Option<String> = sqlx::query_scalar("select state from publication_intents where id = $1 for update")
        .bind(intent_id)
        .fetch_optional(&mut *tx)
        .await?;
    let state = IntentState::parse(&state.ok_or(DomainError::NotFound("publication intent"))?).unwrap_or(IntentState::Scheduled);
    match state {
        IntentState::Scheduled | IntentState::Failed => {
            sqlx::query("update publication_intents set state = 'cancelled', updated_at = now() where id = $1")
                .bind(intent_id)
                .execute(&mut *tx)
                .await?;
        }
        IntentState::Cancelled => {}
        IntentState::Claimed | IntentState::Submitting | IntentState::OutcomeUnknown | IntentState::Accepted => {
            return Err(conflict(
                "submission_started",
                "submission has begun; cancelling requires reconciling with the platform (and deleting the post there if it was published)",
            ));
        }
        IntentState::Published => return Err(conflict("already_published", "the post is live; remove it on the platform")),
    }
    audit(&mut tx, actor, "intent.cancelled", "intent", intent_id, json!({ "from": state })).await?;
    let intent = get_intent(&mut tx, intent_id).await?;
    tx.commit().await?;
    Ok(intent)
}

// ── Guarded dispatch ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum DispatchDecision {
    /// Claim the intent and submit.
    Claim,
    /// Another trigger already claimed or finished it; report, do not submit.
    Converge,
}

pub struct DispatchFacts {
    pub intent_state: IntentState,
    pub intent_revision: i32,
    pub requested_revision: i32,
    pub due_at: DateTime<Utc>,
    pub now: DateTime<Utc>,
    pub early_secs: i64,
    pub window_secs: i64,
    pub paused: bool,
    pub release_state: ReleaseState,
}

/// Pure dispatch guard; every check fails closed.
pub fn dispatch_decision(f: &DispatchFacts) -> DResult<DispatchDecision> {
    match f.intent_state {
        IntentState::Scheduled => {}
        IntentState::Cancelled => return Err(conflict("intent_cancelled", "intent was cancelled")),
        _ => return Ok(DispatchDecision::Converge),
    }
    if f.requested_revision != f.intent_revision {
        return Err(conflict(
            "stale_schedule_revision",
            format!("timer revision {} is not current revision {}", f.requested_revision, f.intent_revision),
        ));
    }
    if f.paused {
        return Err(conflict("production_paused", "publishing is paused"));
    }
    if f.release_state != ReleaseState::Approved {
        return Err(conflict("release_not_approved", format!("release is {}", f.release_state)));
    }
    let early = f.due_at - chrono::Duration::seconds(f.early_secs);
    let late = f.due_at + chrono::Duration::seconds(f.window_secs);
    if f.now < early || f.now > late {
        return Err(conflict("outside_dispatch_window", format!("now {} is outside {early}..{late}", f.now)));
    }
    Ok(DispatchDecision::Claim)
}

pub async fn production_paused(conn: &mut sqlx::PgConnection) -> DResult<bool> {
    let v: Option<Value> = sqlx::query_scalar("select value from settings where key = 'production_pause'").fetch_optional(conn).await?;
    // Missing setting: fail closed.
    Ok(v.map(|v| v["paused"].as_bool().unwrap_or(true)).unwrap_or(true))
}

/// The transport decides the channel. A release frozen before transports were
/// recorded (`transport` absent) keeps the built-in publisher; a transport with
/// no registered channel fails closed, never falls back. Replies select the
/// account's transport through the same function.
pub(crate) fn publisher_for<'a>(
    channels: &'a BTreeMap<String, Arc<dyn Publisher>>,
    builtin: &'a Arc<dyn Publisher>,
    transport: Option<&str>,
) -> DResult<&'a Arc<dyn Publisher>> {
    match transport {
        None => Ok(builtin),
        Some(t) => {
            channels.get(t).ok_or_else(|| conflict("transport_unavailable", format!("no publisher is registered for transport {t:?}")))
        }
    }
}

/// Called by the Windmill publication timer at the due time.
pub async fn dispatch(hub: &Hub, actor: &Actor, intent_id: Uuid, input: DispatchIn) -> DResult<DispatchOut> {
    actor.require(&[Role::Procedure])?;
    // 1. Authoritative approval read before taking any lock; fail closed.
    let (release_id, approval_id): (Uuid, Option<String>) = sqlx::query_as(
        "select r.id, r.paperclip_approval_id from publication_intents i join release_revisions r on r.id = i.release_revision_id where i.id = $1",
    )
    .bind(intent_id)
    .fetch_optional(&hub.db)
    .await?
    .ok_or(DomainError::NotFound("publication intent"))?;

    // Converge early for duplicate timers without touching Paperclip.
    {
        let mut conn = hub.db.acquire().await?;
        let intent = get_intent(&mut conn, intent_id).await?;
        if !matches!(intent.state, IntentState::Scheduled | IntentState::Cancelled) {
            return Ok(DispatchOut { intent, submitted: false });
        }
    }
    let approval_id = approval_id.ok_or_else(|| conflict("release_not_approved", "no native approval is bound"))?;
    let view = hub.paperclip.get_approval(&approval_id).await.map_err(|e| DomainError::Unavailable {
        code: "approval_unverifiable",
        message: format!("cannot verify approval {approval_id}: {e}"),
    })?;

    // 2. Claim atomically, re-checking every condition under the row lock.
    let mut tx = hub.db.begin().await?;
    let row: (String, i32, DateTime<Utc>, String) =
        sqlx::query_as("select state, schedule_revision, due_at, account_id from publication_intents where id = $1 for update")
            .bind(intent_id)
            .fetch_one(&mut *tx)
            .await?;
    let rel = get_release(&mut tx, release_id, false).await?;
    verify_payload(&rel)?;
    let paused = production_paused(&mut tx).await?;
    let decision = dispatch_decision(&DispatchFacts {
        intent_state: IntentState::parse(&row.0).unwrap_or(IntentState::Cancelled),
        intent_revision: row.1,
        requested_revision: input.schedule_revision,
        due_at: row.2,
        now: Utc::now(),
        early_secs: hub.config.dispatch_early_secs,
        window_secs: hub.config.dispatch_window_secs,
        paused,
        release_state: rel.state,
    });
    let decision = match decision {
        Ok(d) => d,
        Err(e) => {
            audit(
                &mut tx,
                actor,
                "intent.dispatch_refused",
                "intent",
                intent_id,
                json!({ "code": e.code(), "trigger_id": input.trigger_id, "schedule_revision": input.schedule_revision }),
            )
            .await?;
            tx.commit().await?;
            return Err(e);
        }
    };
    if decision == DispatchDecision::Converge {
        let intent = get_intent(&mut tx, intent_id).await?;
        tx.commit().await?;
        return Ok(DispatchOut { intent, submitted: false });
    }
    approval_matches(&rel, &view)?;
    if view.status != ApprovalStatus::Approved || view.decided_by_user_id.is_none() {
        return Err(conflict("approval_not_current", format!("native approval is {:?}", view.status)));
    }
    let action = rel
        .actions
        .iter()
        .find(|a| a.account_id == row.3)
        .cloned()
        .ok_or_else(|| DomainError::Internal("intent account not in release".into()))?;
    let attempt: i32 = sqlx::query_scalar("select coalesce(max(attempt), 0) + 1 from delivery_attempts where intent_id = $1")
        .bind(intent_id)
        .fetch_one(&mut *tx)
        .await?;
    let key = format!("ihub-{intent_id}-{attempt}");
    let attempt_id = Uuid::new_v4();
    sqlx::query("insert into delivery_attempts (id, intent_id, attempt, idempotency_key, trigger_id, state) values ($1, $2, $3, $4, $5, 'submitting')")
        .bind(attempt_id)
        .bind(intent_id)
        .bind(attempt)
        .bind(&key)
        .bind(&input.trigger_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "update publication_intents set state = 'submitting', claimed_by = $2, claimed_at = now(), updated_at = now() where id = $1",
    )
    .bind(intent_id)
    .bind(&input.trigger_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        "intent.claimed",
        "intent",
        intent_id,
        json!({ "attempt": attempt, "idempotency_key": key, "trigger_id": input.trigger_id, "approval_id": approval_id }),
    )
    .await?;
    tx.commit().await?;

    // 3. Submit through the guarded transport with the frozen media.
    let platform: String =
        sqlx::query_scalar("select platform from accounts where id = $1").bind(&action.account_id).fetch_one(&hub.db).await?;
    let mut media = vec![];
    for m in &action.media {
        let key: String = sqlx::query_scalar("select storage_key from assets where id = $1 and sha256 = $2 and state = 'ready'")
            .bind(m.asset_id)
            .bind(&m.sha256)
            .fetch_optional(&hub.db)
            .await?
            .ok_or_else(|| DomainError::Internal(format!("frozen media {} missing", m.asset_id)))?;
        let bytes = hub.assets.get(&key).await.map_err(|e| DomainError::Internal(format!("asset store: {e}")))?;
        if hash::sha256_hex(&bytes) != m.sha256 {
            return finish_attempt(hub, intent_id, attempt_id, IntentState::Failed, json!({ "error": "media checksum mismatch" }), None)
                .await;
        }
        media.push(MediaBlob { sha256: m.sha256.clone(), mime: m.mime.clone(), alt_text: m.alt_text.clone(), bytes });
    }
    let req = PublishRequest {
        account_id: action.account_id.clone(),
        platform,
        idempotency_key: key,
        text: action.text.clone(),
        media,
        platform_settings: action.platform_settings.clone(),
        reply_to: action.reply_to.clone(),
    };
    let publisher = match publisher_for(&hub.channels, &hub.publisher, action.transport.as_deref()) {
        Ok(p) => p.clone(),
        Err(e) => {
            // The frozen transport never reached a provider. Terminate the
            // attempt and leave the intent failed for an operator; a channel
            // registered later re-arms it through `schedule`.
            tracing::error!(%intent_id, %e, "frozen transport has no channel; stopping for an operator");
            let out =
                finish_attempt(hub, intent_id, attempt_id, IntentState::Failed, json!({ "code": e.code(), "error": e.to_string() }), None)
                    .await?;
            return Ok(DispatchOut { intent: out.intent, submitted: false });
        }
    };
    match publisher.submit(&req).await {
        Ok(post) => {
            let state = match post.state {
                ProviderState::Published => IntentState::Published,
                ProviderState::Accepted => IntentState::Accepted,
                ProviderState::Failed => IntentState::Failed,
            };
            finish_attempt(hub, intent_id, attempt_id, state, post.raw.clone(), Some(&post)).await
        }
        Err(PortError::Ambiguous(e)) => {
            finish_attempt(hub, intent_id, attempt_id, IntentState::OutcomeUnknown, json!({ "error": e }), None).await
        }
        // Never reached the provider: back to scheduled with a retry timer.
        Err(PortError::Unreachable(e)) => {
            finish_attempt(hub, intent_id, attempt_id, IntentState::Scheduled, json!({ "error": e, "not_sent": true }), None).await
        }
        Err(e) => finish_attempt(hub, intent_id, attempt_id, IntentState::Failed, json!({ "error": e.to_string() }), None).await,
    }
}

const UNREACHABLE_RETRY_SECS: i64 = 60;

async fn finish_attempt(
    hub: &Hub,
    intent_id: Uuid,
    attempt_id: Uuid,
    state: IntentState,
    detail: Value,
    post: Option<&crate::ports::ProviderPost>,
) -> DResult<DispatchOut> {
    let mut tx = hub.db.begin().await?;
    let attempt_state = if state == IntentState::Scheduled { IntentState::Failed } else { state };
    sqlx::query("update delivery_attempts set state = $2, detail = $3, updated_at = now() where id = $1")
        .bind(attempt_id)
        .bind(attempt_state.as_str())
        .bind(&detail)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "update publication_intents set state = $2, provider_post_id = coalesce($3, provider_post_id), provider_url = coalesce($4, provider_url), updated_at = now()
         where id = $1",
    )
    .bind(intent_id)
    .bind(state.as_str())
    .bind(post.map(|p| p.provider_id.clone()))
    .bind(post.and_then(|p| p.url.clone()))
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, &Actor::system(), "intent.delivery", "intent", intent_id, json!({ "state": state, "detail": detail })).await?;
    mark_content_published(&mut tx, intent_id).await?;
    let intent = get_intent(&mut tx, intent_id).await?;
    if state == IntentState::Scheduled {
        // The timer that brought us here is finishing; arm a retry for the same
        // revision in this transaction. The dispatch window bounds the retries.
        let timer = crate::ports::PublicationTimer {
            intent_id: intent_id.to_string(),
            schedule_revision: intent.schedule_revision,
            due_at: Utc::now() + chrono::Duration::seconds(UNREACHABLE_RETRY_SECS),
            dispatch_url: format!("{}/v1/intents/{intent_id}/dispatch", hub.config.public_url),
        };
        outbox::enqueue(
            &mut tx,
            &Actor::system(),
            outbox::WINDMILL_SCHEDULE_PUBLICATION,
            &format!("timer:{intent_id}:{}:retry:{attempt_id}", intent.schedule_revision),
            serde_json::to_value(&timer).unwrap_or_default(),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(DispatchOut { intent, submitted: true })
}

async fn mark_content_published(tx: &mut sqlx::PgConnection, intent_id: Uuid) -> DResult<()> {
    sqlx::query(
        "update content_items c set state = 'published', updated_at = now()
         from release_revisions r join publication_intents i on i.release_revision_id = r.id
         where i.id = $1 and c.id = r.content_item_id
           and not exists (select 1 from publication_intents o where o.release_revision_id = r.id and o.state <> 'published')",
    )
    .bind(intent_id)
    .execute(tx)
    .await?;
    Ok(())
}

/// Intents whose provider result must be confirmed before anything else.
pub async fn intents_to_reconcile(hub: &Hub) -> DResult<Vec<(Uuid, String, String)>> {
    Ok(sqlx::query_as(
        "select i.id, i.account_id, d.idempotency_key from publication_intents i
         join delivery_attempts d on d.intent_id = i.id and d.attempt = (select max(attempt) from delivery_attempts where intent_id = i.id)
         where i.state in ('accepted', 'outcome_unknown') or (i.state = 'submitting' and i.claimed_at < now() - interval '2 minutes')",
    )
    .fetch_all(&hub.db)
    .await?)
}

/// Read under the intent's row lock, in a statement after the lock was taken.
async fn latest_attempt_key(tx: &mut sqlx::PgConnection, intent_id: Uuid) -> DResult<Option<String>> {
    Ok(sqlx::query_scalar("select idempotency_key from delivery_attempts where intent_id = $1 order by attempt desc limit 1")
        .bind(intent_id)
        .fetch_optional(tx)
        .await?)
}

/// Reconcile one intent by its delivery idempotency key.
pub async fn reconcile_intent(hub: &Hub, intent_id: Uuid, account_id: &str, key: &str) -> DResult<()> {
    // Read back through the channel the attempt was submitted on.
    let transport: Option<String> = sqlx::query_scalar(
        "select a->>'transport' from publication_intents i join release_revisions r on r.id = i.release_revision_id,
         jsonb_array_elements(r.actions) a where i.id = $1 and a->>'account_id' = i.account_id",
    )
    .bind(intent_id)
    .fetch_optional(&hub.db)
    .await?
    .flatten();
    let publisher = match publisher_for(&hub.channels, &hub.publisher, transport.as_deref()) {
        Ok(p) => p.clone(),
        Err(e) => {
            tracing::warn!(%intent_id, %e, "no channel to reconcile through");
            return Ok(());
        }
    };
    let found = match publisher.lookup(account_id, key).await {
        Ok(f) => f,
        // No read, or a claim with no platform result (n8n `pending`).
        Err(PortError::Unsupported(_) | PortError::Ambiguous(_)) => {
            // Leave it for an operator to resolve; never guess. Lock the intent
            // first, then confirm `key` is still the latest attempt.
            let mut tx = hub.db.begin().await?;
            let current: String = sqlx::query_scalar("select state from publication_intents where id = $1 for update")
                .bind(intent_id)
                .fetch_one(&mut *tx)
                .await?;
            if current == "submitting" && latest_attempt_key(&mut tx, intent_id).await?.as_deref() == Some(key) {
                sqlx::query("update publication_intents set state = 'outcome_unknown', updated_at = now() where id = $1")
                    .bind(intent_id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(%intent_id, error = %e, "publisher reconcile read failed");
            return Ok(());
        }
    };
    let mut tx = hub.db.begin().await?;
    let current: String =
        sqlx::query_scalar("select state from publication_intents where id = $1 for update").bind(intent_id).fetch_one(&mut *tx).await?;
    // A newer attempt superseded this read; its own reconciliation decides.
    if !matches!(current.as_str(), "accepted" | "outcome_unknown" | "submitting")
        || latest_attempt_key(&mut tx, intent_id).await?.as_deref() != Some(key)
    {
        return Ok(());
    }
    let next = match &found {
        Some(p) if p.state == ProviderState::Published => "published",
        Some(p) if p.state == ProviderState::Failed => "failed",
        // The provider holds a pending record for the latest attempt, but no
        // submit response reached the hub (it crashed after the claim): whether
        // the post exists is unknown. Hand off to an operator; nothing is
        // resubmitted automatically.
        Some(_) if current == "submitting" => "outcome_unknown",
        Some(_) => "accepted",
        // Provider has no record: definitely not published; re-armable via schedule.
        None if current != "accepted" => "failed",
        None => "accepted",
    };
    sqlx::query(
        "update publication_intents set state = $2, provider_post_id = coalesce($3, provider_post_id), provider_url = coalesce($4, provider_url), updated_at = now()
         where id = $1",
    )
    .bind(intent_id)
    .bind(next)
    .bind(found.as_ref().map(|p| p.provider_id.clone()))
    .bind(found.as_ref().and_then(|p| p.url.clone()))
    .execute(&mut *tx)
    .await?;
    sqlx::query("update delivery_attempts set state = $2, updated_at = now() where idempotency_key = $1")
        .bind(key)
        .bind(next)
        .execute(&mut *tx)
        .await?;
    if next != current {
        audit(
            &mut tx,
            &Actor::system(),
            "intent.reconciled",
            "intent",
            intent_id,
            json!({ "from": current, "to": next, "provider": found.map(|p| p.raw) }),
        )
        .await?;
    }
    mark_content_published(&mut tx, intent_id).await?;
    tx.commit().await?;
    Ok(())
}

/// Operator records the outcome after checking the platform by hand.
pub async fn resolve_intent(hub: &Hub, actor: &Actor, intent_id: Uuid, input: ResolveIntentIn) -> DResult<PublicationIntent> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let current: Option<String> = sqlx::query_scalar("select state from publication_intents where id = $1 for update")
        .bind(intent_id)
        .fetch_optional(&mut *tx)
        .await?;
    let current = current.ok_or(DomainError::NotFound("publication intent"))?;
    // `accepted` is resolvable too: a provider without lookup (Postiz) can never confirm it.
    if !matches!(current.as_str(), "outcome_unknown" | "accepted") {
        return Err(conflict("intent_not_unknown", format!("intent is {current}")));
    }
    let next = if input.published { "published" } else { "failed" };
    sqlx::query("update publication_intents set state = $2, provider_post_id = coalesce($3, provider_post_id), provider_url = coalesce($4, provider_url), updated_at = now() where id = $1")
        .bind(intent_id)
        .bind(next)
        .bind(&input.provider_post_id)
        .bind(&input.provider_url)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        actor,
        "intent.resolved",
        "intent",
        intent_id,
        json!({ "published": input.published, "provider_post_id": input.provider_post_id }),
    )
    .await?;
    mark_content_published(&mut tx, intent_id).await?;
    let intent = get_intent(&mut tx, intent_id).await?;
    tx.commit().await?;
    Ok(intent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> DispatchFacts {
        let now = Utc::now();
        DispatchFacts {
            intent_state: IntentState::Scheduled,
            intent_revision: 2,
            requested_revision: 2,
            due_at: now,
            now,
            early_secs: 30,
            window_secs: 900,
            paused: false,
            release_state: ReleaseState::Approved,
        }
    }

    #[test]
    fn dispatch_guard_fails_closed() {
        assert_eq!(dispatch_decision(&facts()).unwrap(), DispatchDecision::Claim);
        let code = |f: DispatchFacts| dispatch_decision(&f).unwrap_err().code();
        assert_eq!(code(DispatchFacts { requested_revision: 1, ..facts() }), "stale_schedule_revision");
        assert_eq!(code(DispatchFacts { paused: true, ..facts() }), "production_paused");
        assert_eq!(code(DispatchFacts { release_state: ReleaseState::Superseded, ..facts() }), "release_not_approved");
        assert_eq!(code(DispatchFacts { intent_state: IntentState::Cancelled, ..facts() }), "intent_cancelled");
        let f = facts();
        assert_eq!(code(DispatchFacts { now: f.due_at - chrono::Duration::seconds(31), ..facts() }), "outside_dispatch_window");
        assert_eq!(code(DispatchFacts { now: f.due_at + chrono::Duration::seconds(901), ..facts() }), "outside_dispatch_window");
        // Duplicate timer after a claim converges without submitting.
        for s in [IntentState::Submitting, IntentState::Accepted, IntentState::Published, IntentState::OutcomeUnknown] {
            assert_eq!(
                dispatch_decision(&DispatchFacts { intent_state: s, requested_revision: 1, ..facts() }).unwrap(),
                DispatchDecision::Converge
            );
        }
    }

    struct Stub(&'static str);

    #[async_trait::async_trait]
    impl Publisher for Stub {
        async fn submit(&self, _: &PublishRequest) -> crate::ports::PortResult<crate::ports::ProviderPost> {
            Err(PortError::Unsupported(self.0.into()))
        }
        async fn lookup(&self, _: &str, _: &str) -> crate::ports::PortResult<Option<crate::ports::ProviderPost>> {
            Ok(None)
        }
        async fn reply(&self, _: &crate::ports::ReplyRequest) -> crate::ports::PortResult<crate::ports::ProviderPost> {
            Err(PortError::Unsupported(self.0.into()))
        }
        async fn lookup_reply(&self, _: &str, _: &str) -> crate::ports::PortResult<Option<crate::ports::ProviderPost>> {
            Ok(None)
        }
    }

    #[test]
    fn a_frozen_transport_selects_its_channel_and_never_falls_back() {
        let builtin: Arc<dyn Publisher> = Arc::new(Stub("builtin"));
        let plinth: Arc<dyn Publisher> = Arc::new(Stub("plinth"));
        let channels: BTreeMap<String, Arc<dyn Publisher>> = BTreeMap::from([("plinth".to_string(), plinth.clone())]);
        // A release frozen before transports were recorded keeps working.
        assert!(Arc::ptr_eq(publisher_for(&channels, &builtin, None).unwrap(), &builtin));
        assert!(Arc::ptr_eq(publisher_for(&channels, &builtin, Some("plinth")).unwrap(), &plinth));
        let err = publisher_for(&channels, &builtin, Some("orphan")).err().unwrap();
        assert_eq!(err.code(), "transport_unavailable");
    }
}
