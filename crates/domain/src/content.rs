//! Content items, briefs, creative chat, Today and search.

use contracts::{
    CanonSource, ChatMessage, ChatMessageIn, ContentDetail, ContentItem, ContentState, CreateBriefIn, SearchHit, Today, TodayItem,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ports::AssistantTurn;
use crate::{Actor, DResult, DomainError, Hub, OPERATOR_OR_AGENT, audit, decode, decode_one, invalid, outbox};

const ITEM_COLS: &str =
    "id, persona_id, news_item_id, title, brief, state, paperclip_issue_id, paperclip_issue_url, created_at, updated_at";

pub(crate) async fn get_item(conn: &mut sqlx::PgConnection, id: Uuid) -> DResult<ContentItem> {
    let row: Option<Value> =
        sqlx::query_scalar(&format!("select to_jsonb(t) from (select {ITEM_COLS} from content_items where id = $1) t"))
            .bind(id)
            .fetch_optional(conn)
            .await?;
    decode_one(row.ok_or(DomainError::NotFound("content item"))?)
}

/// Create a persona-linked brief and ask Paperclip (via the outbox) to create
/// the assigned issue.
pub async fn create_brief(hub: &Hub, actor: &Actor, input: CreateBriefIn) -> DResult<ContentItem> {
    actor.require(OPERATOR_OR_AGENT)?;
    if input.title.trim().is_empty() {
        return Err(invalid("invalid_brief", "title is required"));
    }
    let id = Uuid::new_v4();
    let mut tx = hub.db.begin().await?;
    let persona: Option<String> =
        sqlx::query_scalar("select name from personas where id = $1").bind(input.persona_id).fetch_optional(&mut *tx).await?;
    let persona = persona.ok_or(DomainError::NotFound("persona"))?;
    let news: Option<(String, String)> = match input.news_item_id {
        Some(n) => Some(
            sqlx::query_as("select title, canonical_url from news_items where id = $1")
                .bind(n)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(DomainError::NotFound("news item"))?,
        ),
        None => None,
    };
    sqlx::query("insert into content_items (id, persona_id, news_item_id, title, brief, state) values ($1, $2, $3, $4, $5, 'brief')")
        .bind(id)
        .bind(input.persona_id)
        .bind(input.news_item_id)
        .bind(&input.title)
        .bind(serde_json::to_value(&input.brief).unwrap_or_default())
        .execute(&mut *tx)
        .await?;
    let mut description = format!(
        "Persona: {persona}\nAngle: {}\nAudience: {}\nCopy: {}\nVisual: {}\n\nHub content item: {id}",
        input.brief.angle, input.brief.audience, input.brief.copy_direction, input.brief.visual_direction
    );
    if let Some((title, url)) = &news {
        description.push_str(&format!("\nSource: {title} — {url}"));
    }
    outbox::enqueue(
        &mut tx,
        actor,
        outbox::PAPERCLIP_CREATE_ISSUE,
        &format!("issue:content:{id}"),
        json!({ "content_item_id": id, "company_id": hub.config.company_id, "title": input.title, "description": description, "origin_key": format!("fleetix-hub:content:{id}") }),
    )
    .await?;
    audit(
        &mut tx,
        actor,
        "content.brief_created",
        "content",
        id,
        json!({ "news_item_id": input.news_item_id, "persona_id": input.persona_id }),
    )
    .await?;
    let item = get_item(&mut tx, id).await?;
    tx.commit().await?;
    Ok(item)
}

pub async fn update_brief(hub: &Hub, actor: &Actor, id: Uuid, brief: contracts::Brief) -> DResult<ContentItem> {
    actor.require(OPERATOR_OR_AGENT)?;
    let mut tx = hub.db.begin().await?;
    let n = sqlx::query("update content_items set brief = $2, updated_at = now() where id = $1")
        .bind(id)
        .bind(serde_json::to_value(&brief).unwrap_or_default())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(DomainError::NotFound("content item"));
    }
    audit(&mut tx, actor, "content.brief_updated", "content", id, serde_json::to_value(&brief).unwrap_or_default()).await?;
    let item = get_item(&mut tx, id).await?;
    tx.commit().await?;
    Ok(item)
}

pub async fn list(hub: &Hub, actor: &Actor) -> DResult<Vec<ContentItem>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {ITEM_COLS} from content_items order by updated_at desc limit 200) t"
    ))
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

pub async fn detail(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<ContentDetail> {
    actor.require(crate::ANY_READER)?;
    let mut conn = hub.db.acquire().await?;
    let item = get_item(&mut conn, id).await?;
    drop(conn);
    Ok(ContentDetail {
        item,
        generations: crate::production::list_for_content(hub, id).await?,
        assets: crate::production::assets_for_content(hub, id).await?,
        releases: crate::release::list_for_content(hub, id).await?,
        intents: crate::release::intents_for_content(hub, id).await?,
        history: crate::history(&hub.db, "content", &id.to_string()).await?,
    })
}

pub(crate) async fn set_state(conn: &mut sqlx::PgConnection, id: Uuid, state: ContentState) -> DResult<()> {
    sqlx::query("update content_items set state = $2, updated_at = now() where id = $1")
        .bind(id)
        .bind(state.as_str())
        .execute(conn)
        .await?;
    Ok(())
}

// ── Creative chat ───────────────────────────────────────────────────────────

pub async fn chat(hub: &Hub, actor: &Actor, subject_type: &str, subject_id: Uuid) -> DResult<Vec<ChatMessage>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(t) from (select id, subject_type, subject_id, role, author, body, proposal_id, created_at
         from chat_messages where subject_type = $1 and subject_id = $2 order by created_at, id) t",
    )
    .bind(subject_type)
    .bind(subject_id)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

/// Resolve the persona a chat subject belongs to, plus the context the
/// assistant sees (brief, accepted canon, linked Paperclip issue).
async fn chat_context(hub: &Hub, actor: &Actor, subject_type: &str, subject_id: Uuid) -> DResult<(Uuid, Value)> {
    let (persona_id, item) = match subject_type {
        "content" => {
            let mut conn = hub.db.acquire().await?;
            let item = get_item(&mut conn, subject_id).await?;
            (item.persona_id, Some(item))
        }
        "persona" => (subject_id, None),
        _ => return Err(invalid("invalid_subject", "chat subject must be content or persona")),
    };
    let persona = crate::canon::get_persona(hub, actor, persona_id).await?;
    Ok((
        persona_id,
        json!({
            "persona": { "name": persona.name, "canon": persona.canon },
            "content": item.as_ref().map(|i| json!({ "title": i.title, "brief": i.brief, "state": i.state, "paperclip_issue_id": i.paperclip_issue_id })),
        }),
    ))
}

pub async fn send_chat(hub: &Hub, actor: &Actor, subject_type: &str, subject_id: Uuid, input: ChatMessageIn) -> DResult<Vec<ChatMessage>> {
    actor.require(OPERATOR_OR_AGENT)?;
    if input.body.trim().is_empty() {
        return Err(invalid("invalid_chat", "message is empty"));
    }
    let (persona_id, context) = chat_context(hub, actor, subject_type, subject_id).await?;
    let role = if actor.principal.role == contracts::Role::Agent { "assistant" } else { "operator" };
    sqlx::query("insert into chat_messages (id, subject_type, subject_id, role, author, body) values ($1, $2, $3, $4, $5, $6)")
        .bind(Uuid::new_v4())
        .bind(subject_type)
        .bind(subject_id)
        .bind(role)
        .bind(actor.name())
        .bind(&input.body)
        .execute(&hub.db)
        .await?;
    let turns: Vec<AssistantTurn> =
        chat(hub, actor, subject_type, subject_id).await?.into_iter().map(|m| AssistantTurn { role: m.role, body: m.body }).collect();
    let reply = match hub.assistant.reply(&context, &turns).await {
        Ok(r) => r,
        Err(e) => {
            sqlx::query(
                "insert into chat_messages (id, subject_type, subject_id, role, author, body) values ($1, $2, $3, 'system', 'hub', $4)",
            )
            .bind(Uuid::new_v4())
            .bind(subject_type)
            .bind(subject_id)
            .bind(format!("assistant unavailable: {e}"))
            .execute(&hub.db)
            .await?;
            return chat(hub, actor, subject_type, subject_id).await;
        }
    };
    // A suggestion becomes a reviewable proposal; accepted canon is untouched.
    let proposal_id = match reply.canon_suggestion {
        Some(body) => {
            let base = context["persona"]["canon"]["revision"].as_i64().map(|r| r as i32);
            let assistant_actor = Actor {
                principal: contracts::Principal { id: Uuid::nil(), name: reply.model.clone(), role: contracts::Role::Agent },
                correlation_id: actor.correlation_id.clone(),
            };
            let rev = crate::canon::propose(
                hub,
                &assistant_actor,
                persona_id,
                contracts::CanonProposalIn {
                    base_revision: base,
                    body,
                    source: CanonSource::Chat,
                    rationale: Some(format!("suggested in {subject_type} chat")),
                },
            )
            .await?;
            Some(rev.id)
        }
        None => None,
    };
    sqlx::query("insert into chat_messages (id, subject_type, subject_id, role, author, body, proposal_id) values ($1, $2, $3, 'assistant', $4, $5, $6)")
        .bind(Uuid::new_v4())
        .bind(subject_type)
        .bind(subject_id)
        .bind(format!("assistant:{}", reply.model))
        .bind(&reply.body)
        .bind(proposal_id)
        .execute(&hub.db)
        .await?;
    chat(hub, actor, subject_type, subject_id).await
}

// ── Today & search ──────────────────────────────────────────────────────────

async fn today_items(hub: &Hub, sql: &str) -> DResult<Vec<TodayItem>> {
    let rows: Vec<Value> = sqlx::query_scalar(&format!("select to_jsonb(t) from ({sql} limit 50) t")).fetch_all(&hub.db).await?;
    decode(rows)
}

pub async fn today(hub: &Hub, actor: &Actor) -> DResult<Today> {
    actor.require(crate::ANY_READER)?;
    Ok(Today {
        pending_reviews: today_items(
            hub,
            "select 'release_review' as kind, c.title, 'content' as subject_type, c.id as subject_id, r.state as status, r.created_at as at
             from release_revisions r join content_items c on c.id = r.content_item_id where r.state in ('draft', 'in_review')
             union all
             select 'canon_proposal', p.name || ' r' || cr.revision, 'persona', p.id, cr.state, cr.created_at
             from canon_revisions cr join personas p on p.id = cr.persona_id where cr.state = 'proposed'
             union all
             select 'reply_review', rel.counterpart, 'relationship', rel.id, d.state, d.created_at
             from reply_drafts d join relationships rel on rel.id = d.relationship_id where d.state in ('draft', 'in_review')
             order by at",
        )
        .await?,
        active_production: today_items(
            hub,
            "select 'generation' as kind, c.title, 'content' as subject_type, c.id as subject_id, g.state as status, g.updated_at as at
             from generation_requests g join content_items c on c.id = g.content_item_id
             where g.state in ('requested', 'dispatched', 'running', 'cancelling') order by g.updated_at desc",
        )
        .await?,
        due_releases: today_items(
            hub,
            "select 'publication' as kind, c.title || ' → ' || i.account_id as title, 'content' as subject_type, c.id as subject_id, i.state as status, i.due_at as at
             from publication_intents i join release_revisions r on r.id = i.release_revision_id join content_items c on c.id = r.content_item_id
             where i.state in ('scheduled', 'claimed', 'submitting', 'accepted') and i.due_at < now() + interval '24 hours' order by i.due_at",
        )
        .await?,
        failed_operations: today_items(
            hub,
            "select 'publication' as kind, c.title || ' → ' || i.account_id as title, 'content' as subject_type, c.id as subject_id, i.state as status, i.updated_at as at
             from publication_intents i join release_revisions r on r.id = i.release_revision_id join content_items c on c.id = r.content_item_id
             where i.state in ('failed', 'outcome_unknown')
             union all
             select 'generation', c.title, 'content', c.id, g.state, g.updated_at
             from generation_requests g join content_items c on c.id = g.content_item_id where g.state in ('failed', 'outcome_unknown')
             union all
             select 'outbox:' || o.topic, coalesce(o.last_error, o.topic), 'outbox', o.id, o.state, o.created_at
             from outbox o where o.state = 'dead' or (o.state = 'pending' and o.attempts >= 3)
             union all
             select 'reply', rel.counterpart, 'relationship', rel.id, d.state, d.updated_at
             from reply_drafts d join relationships rel on rel.id = d.relationship_id where d.state = 'outcome_unknown'
             order by at desc",
        )
        .await?,
        overdue_interactions: today_items(
            hub,
            "select 'follow_up' as kind, r.counterpart as title, 'relationship' as subject_type, r.id as subject_id, r.owner as status,
               coalesce(r.follow_up_at, r.last_inbound_at) as at
             from relationships r
             where r.follow_up_at < now()
                or (r.last_inbound_at < now() - interval '4 hours' and not exists (
                     select 1 from interactions i where i.relationship_id = r.id and i.direction = 'outbound' and i.occurred_at > r.last_inbound_at))
             order by at",
        )
        .await?,
    })
}

pub async fn search(hub: &Hub, actor: &Actor, q: &str) -> DResult<Vec<SearchHit>> {
    actor.require(crate::ANY_READER)?;
    if q.trim().is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(t) from (
           select 'news' as subject_type, id as subject_id, title, ts_headline('simple', summary, q) as snippet, ts_rank(search, q) as rank
           from news_items, websearch_to_tsquery('simple', $1) q where search @@ q
           union all
           select 'content', id, title, ts_headline('simple', brief::text, q), ts_rank(search, q)
           from content_items, websearch_to_tsquery('simple', $1) q where search @@ q
           order by rank desc limit 30) t",
    )
    .bind(q)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}
