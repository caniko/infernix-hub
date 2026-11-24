//! Recall grounded in persona, truth tier and (for fan memories) relationship.

use std::{cmp::Ordering, collections::HashSet};

use chrono::Utc;
use contracts::{
    CreateSoulMemoryIn, CreateVoiceExemplarIn, MemoryTier, NoveltyCheckIn, NoveltyCheckOut, PersonaState, RetrieveSoulContextIn,
    SoulContextOut, SoulMemory, SoulMemoryHit, UpsertPersonaStateIn, VoiceExemplar, VoiceMode,
};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{ANY_READER, Actor, DResult, DomainError, Hub, OPERATOR, audit, decode, decode_one, invalid};

const DEFAULT_NOVELTY_THRESHOLD: f32 = 0.82;

async fn require_persona(hub: &Hub, id: Uuid) -> DResult<()> {
    let exists: bool = sqlx::query_scalar("select exists(select 1 from personas where id = $1 and company_id = $2)")
        .bind(id)
        .bind(&hub.config.company_id)
        .fetch_one(&hub.db)
        .await?;
    if !exists {
        return Err(DomainError::NotFound("persona"));
    }
    Ok(())
}

fn validate_embedding(embedding: Option<&[f32]>) -> DResult<()> {
    if embedding.is_some_and(|v| v.len() > 4096 || v.iter().any(|x| !x.is_finite())) {
        return Err(invalid("invalid_embedding", "embedding must contain at most 4096 finite numbers"));
    }
    Ok(())
}

pub async fn create_memory(hub: &Hub, actor: &Actor, persona_id: Uuid, mut input: CreateSoulMemoryIn) -> DResult<SoulMemory> {
    actor.require(OPERATOR)?;
    require_persona(hub, persona_id).await?;
    validate_embedding(input.embedding.as_deref())?;
    let salience = input.salience.unwrap_or(0.5);
    if !(0.0..=1.0).contains(&salience) {
        return Err(invalid("invalid_salience", "salience must be between 0 and 1"));
    }
    if input.body.trim().is_empty() || input.body.len() > 65_536 || input.title.len() > 1024 {
        return Err(invalid("invalid_memory", "body must be non-empty and at most 65536 bytes; title at most 1024 bytes"));
    }
    if input.keywords.len() > 64 || input.keywords.iter().any(|k| k.trim().is_empty() || k.len() > 256) {
        return Err(invalid("invalid_keywords", "use at most 64 non-empty keywords of at most 256 bytes each"));
    }
    input.subjects.push(persona_id);
    input.subjects.sort_unstable();
    input.subjects.dedup();
    let mut tx = hub.db.begin().await?;
    let subjects: i64 = sqlx::query_scalar("select count(*) from personas where id = any($1) and company_id = $2")
        .bind(&input.subjects)
        .bind(&hub.config.company_id)
        .fetch_one(&mut *tx)
        .await?;
    if subjects != input.subjects.len() as i64 {
        return Err(invalid("invalid_subjects", "all memory subjects must be personas in this Hub"));
    }
    if let Some(id) = input.relationship_id {
        let belongs: bool = sqlx::query_scalar(
            "select exists(select 1 from relationships r join accounts a on a.id = r.account_id
             where r.id = $1 and a.company_id = $2 and a.persona_id = any($3))",
        )
        .bind(id)
        .bind(&hub.config.company_id)
        .bind(&input.subjects)
        .fetch_one(&mut *tx)
        .await?;
        if !belongs {
            return Err(invalid("invalid_relationship", "relationship must belong to an account for a memory subject"));
        }
    }
    let id = Uuid::new_v4();
    let row = sqlx::query_scalar(
        "insert into soul_memories as m (id, subjects, tier, relationship_id, title, body, keywords, salience, embedding, occurred_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, coalesce($10, now())) returning to_jsonb(m) - 'search_tsv'",
    )
    .bind(id)
    .bind(&input.subjects)
    .bind(input.tier.as_str())
    .bind(input.relationship_id)
    .bind(&input.title)
    .bind(&input.body)
    .bind(&input.keywords)
    .bind(salience)
    .bind(input.embedding)
    .bind(input.occurred_at)
    .fetch_one(&mut *tx)
    .await?;
    audit(&mut tx, actor, "soul.memory_created", "persona", persona_id, json!({ "memory_id": id, "tier": input.tier })).await?;
    let memory = decode_one(row)?;
    tx.commit().await?;
    Ok(memory)
}

fn recall_tiers(include_sandbox: bool) -> &'static [&'static str] {
    if include_sandbox { &["canon", "aired", "sandbox"] } else { &["canon", "aired"] }
}

pub async fn list_memories(hub: &Hub, actor: &Actor, persona_id: Uuid, include_sandbox: bool) -> DResult<Vec<SoulMemory>> {
    actor.require(ANY_READER)?;
    require_persona(hub, persona_id).await?;
    let rows = sqlx::query_scalar(
        "select to_jsonb(m) - 'search_tsv' from soul_memories m
         where subjects @> array[$1]::uuid[] and tier = any($2)
         order by occurred_at desc, id desc limit 200",
    )
    .bind(persona_id)
    .bind(recall_tiers(include_sandbox))
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

pub async fn create_voice_exemplar(hub: &Hub, actor: &Actor, persona_id: Uuid, input: CreateVoiceExemplarIn) -> DResult<VoiceExemplar> {
    actor.require(OPERATOR)?;
    require_persona(hub, persona_id).await?;
    validate_embedding(input.embedding.as_deref())?;
    if input.rewrite.trim().is_empty() || input.rewrite.len() > 2048 || input.draft.as_ref().is_some_and(|draft| draft.len() > 2048) {
        return Err(invalid("invalid_exemplar", "rewrite must be non-empty; draft and rewrite may be at most 2048 bytes each"));
    }
    let id = Uuid::new_v4();
    let mut tx = hub.db.begin().await?;
    let row = sqlx::query_scalar(
        "insert into soul_voice_exemplars as e (id, persona_id, mode, draft, rewrite, embedding, approved_by)
         values ($1, $2, $3, $4, $5, $6, $7) returning to_jsonb(e)",
    )
    .bind(id)
    .bind(persona_id)
    .bind(input.mode.as_str())
    .bind(input.draft)
    .bind(input.rewrite)
    .bind(input.embedding)
    .bind(actor.name())
    .fetch_one(&mut *tx)
    .await?;
    audit(&mut tx, actor, "soul.exemplar_approved", "persona", persona_id, json!({ "exemplar_id": id, "mode": input.mode })).await?;
    let exemplar = decode_one(row)?;
    tx.commit().await?;
    Ok(exemplar)
}

async fn exemplars(conn: &mut PgConnection, persona_id: Uuid, mode: Option<VoiceMode>, limit: i64) -> DResult<Vec<VoiceExemplar>> {
    let rows = sqlx::query_scalar(
        "select to_jsonb(e) from soul_voice_exemplars e where persona_id = $1 and ($2::text is null or mode = $2)
         order by created_at desc, id desc limit $3",
    )
    .bind(persona_id)
    .bind(mode.map(VoiceMode::as_str))
    .bind(limit)
    .fetch_all(conn)
    .await?;
    decode(rows)
}

pub async fn list_voice_exemplars(hub: &Hub, actor: &Actor, persona_id: Uuid, mode: Option<VoiceMode>) -> DResult<Vec<VoiceExemplar>> {
    actor.require(ANY_READER)?;
    require_persona(hub, persona_id).await?;
    exemplars(&mut *hub.db.acquire().await?, persona_id, mode, 200).await
}

pub async fn upsert_persona_state(hub: &Hub, actor: &Actor, persona_id: Uuid, input: UpsertPersonaStateIn) -> DResult<PersonaState> {
    actor.require(OPERATOR)?;
    require_persona(hub, persona_id).await?;
    let mut tx = hub.db.begin().await?;
    let row = sqlx::query_scalar(
        "insert into soul_persona_state as s (id, persona_id, state_day, mood, obsessions, ruptures, extra)
         values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (persona_id, state_day) do update set mood = excluded.mood, obsessions = excluded.obsessions,
           ruptures = excluded.ruptures, extra = excluded.extra, updated_at = now()
         returning to_jsonb(s)",
    )
    .bind(Uuid::new_v4())
    .bind(persona_id)
    .bind(input.state_day)
    .bind(input.mood)
    .bind(sqlx::types::Json(input.obsessions))
    .bind(sqlx::types::Json(input.ruptures))
    .bind(sqlx::types::Json(input.extra))
    .fetch_one(&mut *tx)
    .await?;
    let state: PersonaState = decode_one(row)?;
    audit(&mut tx, actor, "soul.state_upserted", "persona", persona_id, json!({ "state_day": input.state_day })).await?;
    tx.commit().await?;
    Ok(state)
}

async fn latest_state(conn: &mut PgConnection, persona_id: Uuid) -> DResult<Option<PersonaState>> {
    sqlx::query_scalar("select to_jsonb(s) from soul_persona_state s where persona_id = $1 order by state_day desc limit 1")
        .bind(persona_id)
        .fetch_optional(conn)
        .await?
        .map(decode_one)
        .transpose()
}

pub async fn latest_persona_state(hub: &Hub, actor: &Actor, persona_id: Uuid) -> DResult<Option<PersonaState>> {
    actor.require(ANY_READER)?;
    require_persona(hub, persona_id).await?;
    latest_state(&mut *hub.db.acquire().await?, persona_id).await
}

pub async fn retrieve_context(hub: &Hub, actor: &Actor, persona_id: Uuid, input: RetrieveSoulContextIn) -> DResult<SoulContextOut> {
    actor.require(ANY_READER)?;
    require_persona(hub, persona_id).await?;
    validate_embedding(input.embedding.as_deref())?;
    if input.query.len() > 32_768 {
        return Err(invalid("invalid_recall_query", "query may be at most 32768 bytes"));
    }
    let mut conn = hub.db.acquire().await?;
    // Match any stemmed term: a multi-message query rarely occurs verbatim in one memory.
    // ponytail: rerank every scoped memory until the corpus warrants bounded indexed candidate pools.
    let rows: Vec<(Value, f32)> = sqlx::query_as(
        "with terms as (
           select string_agg(quote_literal(term), ' | ')::tsquery as q
           from unnest(tsvector_to_array(to_tsvector('english', $4))) term
         )
         select to_jsonb(m) - 'search_tsv', coalesce(ts_rank_cd(lex.search_tsv, terms.q, 32), 0::real)
         from soul_memories m cross join terms
         left join soul_memories lex on lex.id = m.id and lex.search_tsv @@ terms.q
         where m.subjects @> array[$1]::uuid[] and m.tier = any($2)
           and (m.relationship_id is null or m.relationship_id = $3)",
    )
    .bind(persona_id)
    .bind(recall_tiers(input.include_sandbox))
    .bind(input.relationship_id)
    .bind(&input.query)
    .fetch_all(&mut *conn)
    .await?;
    let lowered_query = input.query.to_lowercase();
    let now = Utc::now();
    let mut memories = rows
        .into_iter()
        .map(|(row, lexical)| {
            let memory: SoulMemory = decode_one(row)?;
            let keyword_match = keyword_match(&lowered_query, &memory.keywords);
            let semantic = embedding_similarity(input.embedding.as_deref(), memory.embedding.as_deref()).unwrap_or(0.0);
            let age_days = (now - memory.occurred_at).num_seconds().max(0) as f32 / 86400.0;
            let recency = 1.0 / (1.0 + age_days / 30.0);
            let score = 0.45 * lexical + 0.35 * semantic + 0.12 * recency + 0.08 * memory.salience;
            Ok(SoulMemoryHit { memory, keyword_match, score })
        })
        .collect::<DResult<Vec<_>>>()?;
    memories.sort_unstable_by(|a, b| {
        recall_priority(a.keyword_match, a.score, b.keyword_match, b.score)
            .then_with(|| b.memory.occurred_at.cmp(&a.memory.occurred_at))
            .then_with(|| a.memory.id.cmp(&b.memory.id))
    });
    memories.truncate(input.limit.unwrap_or(8).clamp(1, 50));
    let state = latest_state(&mut conn, persona_id).await?;
    let exemplars = exemplars(&mut conn, persona_id, Some(input.mode), 8).await?;
    Ok(SoulContextOut { state, memories, exemplars })
}

fn keyword_match(lowered_query: &str, keywords: &[String]) -> bool {
    keywords.iter().map(|k| k.trim()).any(|k| !k.is_empty() && lowered_query.contains(&k.to_lowercase()))
}

fn recall_priority(a_keyword: bool, a_score: f32, b_keyword: bool, b_score: f32) -> Ordering {
    b_keyword.cmp(&a_keyword).then_with(|| b_score.total_cmp(&a_score))
}

fn embedding_similarity(a: Option<&[f32]>, b: Option<&[f32]>) -> Option<f32> {
    cosine(a?, b?)
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let (mut dot, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (&a, &b) in a.iter().zip(b) {
        let (a, b) = (f64::from(a), f64::from(b));
        dot += a * b;
        aa += a * a;
        bb += b * b;
    }
    let similarity = dot / (aa.sqrt() * bb.sqrt());
    similarity.is_finite().then(|| similarity.clamp(-1.0, 1.0) as f32)
}

pub async fn check_hook_novelty(hub: &Hub, actor: &Actor, persona_id: Option<Uuid>, input: NoveltyCheckIn) -> DResult<NoveltyCheckOut> {
    actor.require(ANY_READER)?;
    if let Some(id) = persona_id {
        require_persona(hub, id).await?;
    }
    validate_embedding(input.embedding.as_deref())?;
    let threshold = input.threshold.unwrap_or(DEFAULT_NOVELTY_THRESHOLD);
    if !(0.0..=1.0).contains(&threshold) {
        return Err(invalid("invalid_novelty_threshold", "threshold must be between 0 and 1"));
    }
    let terms = words(&input.text);
    if terms.is_empty() || input.text.len() > 32_768 {
        return Err(invalid("invalid_hook", "text must contain words and be at most 32768 bytes"));
    }
    // ponytail: scan aired hooks exactly; use an indexed candidate set if the corpus outgrows exact recall.
    let rows: Vec<(Uuid, String, String, Option<Vec<f32>>)> = sqlx::query_as(
        "select id, title, body, embedding from soul_memories where tier = $1
         and ($2::uuid is null or subjects @> array[$2]::uuid[])",
    )
    .bind(MemoryTier::Aired.as_str())
    .bind(persona_id)
    .fetch_all(&hub.db)
    .await?;
    let closest = rows
        .into_iter()
        .map(|(id, title, body, embedding)| {
            let similarity = embedding_similarity(input.embedding.as_deref(), embedding.as_deref())
                .unwrap_or_else(|| jaccard(&terms, &words(&title)).max(jaccard(&terms, &words(&body))));
            (similarity, id, title)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    Ok(novelty_out(closest, threshold))
}

fn words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_lowercase).collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    let shared = a.intersection(b).count();
    shared as f32 / (a.len() + b.len() - shared).max(1) as f32
}

fn novelty_out(closest: Option<(f32, Uuid, String)>, threshold: f32) -> NoveltyCheckOut {
    let (max_similarity, closest_memory_id, closest_title) =
        closest.map_or((0.0, None, None), |(similarity, id, title)| (similarity, Some(id), Some(title)));
    NoveltyCheckOut { is_novel: max_similarity < threshold, max_similarity, closest_memory_id, closest_title }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_requires_matching_nonzero_vectors_and_preserves_direction() {
        assert_eq!(cosine(&[2.0, 0.0], &[3.0, 0.0]), Some(1.0));
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
        assert_eq!(cosine(&[1.0], &[-1.0]), Some(-1.0));
        assert_eq!(cosine(&[], &[]), None);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), None);
        assert_eq!(cosine(&[0.0], &[1.0]), None);
        assert_eq!(cosine(&[f32::NAN], &[1.0]), None);
    }

    #[test]
    fn literal_lorebook_keys_outrank_hybrid_scores() {
        assert!(keyword_match("the [rain] returns", &["[RAIN]".into()]));
        assert!(!keyword_match("rain returns", &["[RAIN]".into(), " ".into()]));
        assert!(recall_priority(true, 0.0, false, 1.0).is_lt());
        assert!(recall_priority(false, 0.1, false, 0.2).is_gt());
    }

    #[test]
    fn default_tier_fence_only_admits_canon_and_aired() {
        assert_eq!(recall_tiers(false), ["canon", "aired"]);
        assert_eq!(recall_tiers(true), ["canon", "aired", "sandbox"]);
    }

    #[test]
    fn novelty_gate_uses_an_inclusive_duplicate_threshold_and_word_sets() {
        assert_eq!(jaccard(&words("Rain, rain again!"), &words("again RAIN")), 1.0);
        assert_eq!(jaccard(&words("Rain on glass"), &words("glass")), 1.0 / 3.0);
        let id = Uuid::new_v4();
        let duplicate = novelty_out(Some((DEFAULT_NOVELTY_THRESHOLD, id, "Aired hook".into())), DEFAULT_NOVELTY_THRESHOLD);
        assert!(!duplicate.is_novel);
        assert_eq!(duplicate.closest_memory_id, Some(id));
        assert_eq!(duplicate.closest_title.as_deref(), Some("Aired hook"));
        let empty = novelty_out(None, DEFAULT_NOVELTY_THRESHOLD);
        assert!(empty.is_novel);
        assert_eq!(empty.max_similarity, 0.0);
        assert_eq!(empty.closest_memory_id, None);
    }
}
