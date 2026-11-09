//! News ingest: persisted before acknowledgement, deduplicated per sender
//! event and per canonical URL, revised when the content changes.

use contracts::{IngestReceipt, NewsEventIn, NewsItem, Role};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, Hub, audit, decode, hash, invalid};

pub const INGESTERS: &[Role] = &[Role::Ingest, Role::Procedure, Role::Operator];

/// Normalize a URL for duplicate detection: lowercase scheme/host, drop the
/// fragment, tracking parameters and a trailing slash.
pub fn canonical_url(raw: &str) -> String {
    let raw = raw.trim();
    let (no_frag, _) = raw.split_once('#').unwrap_or((raw, ""));
    let (base, query) = no_frag.split_once('?').unwrap_or((no_frag, ""));
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, String::new()));
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = path.trim_end_matches('/');
    let mut params: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let k = p.split('=').next().unwrap_or("");
            !(k.starts_with("utm_") || matches!(k, "fbclid" | "gclid" | "ref" | "mc_cid" | "mc_eid"))
        })
        .collect();
    params.sort_unstable();
    let mut out = format!("{}://{host}{path}", scheme.to_lowercase());
    if !params.is_empty() {
        out.push('?');
        out.push_str(&params.join("&"));
    }
    out
}

pub async fn ingest(hub: &Hub, actor: &Actor, ev: NewsEventIn, idempotency_key: Option<String>) -> DResult<IngestReceipt> {
    actor.require(INGESTERS)?;
    if ev.source.trim().is_empty() || ev.external_id.trim().is_empty() || ev.url.trim().is_empty() {
        return Err(invalid("invalid_news", "source, external_id and url are required"));
    }
    let key = idempotency_key.unwrap_or_else(|| ev.external_id.clone());
    let mut tx = hub.db.begin().await?;

    // Persist the delivery first; a retry of the same event converges here.
    let inserted: bool = sqlx::query_scalar(
        "insert into inbox (id, source, idempotency_key, kind, payload) values ($1, $2, $3, 'news', $4)
         on conflict (source, idempotency_key) do update set deliveries = inbox.deliveries + 1
         returning (xmax = 0)",
    )
    .bind(Uuid::new_v4())
    .bind(&ev.source)
    .bind(&key)
    .bind(serde_json::to_value(&ev).unwrap_or(Value::Null))
    .fetch_one(&mut *tx)
    .await?;

    if !inserted
        && let Some((id, revision)) = sqlx::query_as::<_, (Uuid, i32)>(
            "update news_sightings s set deliveries = s.deliveries + 1 from news_items n
             where n.id = s.news_item_id and s.source = $1 and s.external_id = $2
             returning n.id, n.revision",
        )
        .bind(&ev.source)
        .bind(&ev.external_id)
        .fetch_optional(&mut *tx)
        .await?
    {
        tx.commit().await?;
        return Ok(IngestReceipt { id, created: false, revision });
    }

    let url = canonical_url(&ev.url);
    let (id, created): (Uuid, bool) = sqlx::query_as(
        "insert into news_items (id, canonical_url, title, summary) values ($1, $2, $3, $4)
         on conflict (canonical_url) do update set canonical_url = excluded.canonical_url
         returning id, (xmax = 0)",
    )
    .bind(Uuid::new_v4())
    .bind(&url)
    .bind(&ev.title)
    .bind(&ev.summary)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "insert into news_sightings (news_item_id, source, external_id) values ($1, $2, $3)
         on conflict (source, external_id) do update set deliveries = news_sightings.deliveries + 1",
    )
    .bind(id)
    .bind(&ev.source)
    .bind(&ev.external_id)
    .execute(&mut *tx)
    .await?;

    let content = json!({ "title": ev.title, "summary": ev.summary, "evidence": ev.evidence, "published_at": ev.published_at });
    let content_sha = hash::sha256_hex(hash::canonical_json(&content).as_bytes());
    let payload = json!({ "content": content, "source": ev.source, "external_id": ev.external_id, "url": ev.url, "raw": ev.raw });
    let new_revision: Option<i32> = sqlx::query_scalar(
        "insert into news_revisions (id, news_item_id, revision, content_sha256, payload)
         select $1, $2, coalesce(max(revision), 0) + 1, $3, $4 from news_revisions where news_item_id = $2
         on conflict (news_item_id, content_sha256) do nothing
         returning revision",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .bind(&content_sha)
    .bind(&payload)
    .fetch_optional(&mut *tx)
    .await?;

    let revision = match new_revision {
        Some(rev) => {
            sqlx::query("update news_items set title = $2, summary = $3, revision = $4 where id = $1")
                .bind(id)
                .bind(&ev.title)
                .bind(&ev.summary)
                .bind(rev)
                .execute(&mut *tx)
                .await?;
            rev
        }
        None => sqlx::query_scalar("select revision from news_items where id = $1").bind(id).fetch_one(&mut *tx).await?,
    };
    audit(
        &mut tx,
        actor,
        if created { "news.created" } else { "news.sighted" },
        "news",
        id,
        json!({ "source": ev.source, "external_id": ev.external_id, "revision": revision }),
    )
    .await?;
    tx.commit().await?;
    Ok(IngestReceipt { id, created, revision })
}

const NEWS_SELECT: &str = "select to_jsonb(t) from (
    select n.id, n.canonical_url, n.title, n.summary, n.revision, n.first_seen_at,
      (select coalesce(jsonb_agg(distinct s.source), '[]') from news_sightings s where s.news_item_id = n.id) as sources,
      (select coalesce(sum(s.deliveries), 0) from news_sightings s where s.news_item_id = n.id) as coverage,
      coalesce((select r.payload->'content'->'evidence' from news_revisions r where r.news_item_id = n.id order by r.revision desc limit 1), '[]') as evidence,
      (select c.id from content_items c where c.news_item_id = n.id order by c.created_at desc limit 1) as content_item_id
    from news_items n";

pub async fn list(hub: &Hub, actor: &Actor) -> DResult<Vec<NewsItem>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> =
        sqlx::query_scalar(&format!("{NEWS_SELECT} order by n.first_seen_at desc limit 200) t")).fetch_all(&hub.db).await?;
    decode(rows)
}

pub async fn get(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<NewsItem> {
    actor.require(crate::ANY_READER)?;
    let row: Option<Value> = sqlx::query_scalar(&format!("{NEWS_SELECT} where n.id = $1) t")).bind(id).fetch_optional(&hub.db).await?;
    crate::decode_one(row.ok_or(crate::DomainError::NotFound("news item"))?)
}

#[cfg(test)]
mod tests {
    use super::canonical_url;

    #[test]
    fn canonical_url_collapses_tracking_and_case() {
        let a = canonical_url("HTTPS://www.Example.com/a/b/?utm_source=x&id=2&fbclid=9#top");
        assert_eq!(a, "https://example.com/a/b?id=2");
        assert_eq!(a, canonical_url("https://example.com/a/b?id=2"));
        assert_ne!(a, canonical_url("https://example.com/a/b?id=3"));
    }
}
