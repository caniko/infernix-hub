//! Notifications: one row per condition, level-triggered by a producer and
//! owned by the operator afterwards.
//!
//! A producer's only write is the upsert: a condition that is still true and
//! unchanged touches nothing (no popup, and an acknowledgement survives), while
//! a changed or returning one reopens and bumps `seq`. `sync` closes the set by
//! resolving the rows a source no longer reports. `seq` is the long-poll cursor,
//! so every change here wakes the waiters after its transaction commits.

use chrono::Utc;
use contracts::{AckNotificationsIn, Notification, NotificationPage, RaiseNotificationIn, Role, Severity, SnoozeNotificationIn};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR, audit, decode, decode_one, invalid};

/// Producer keys belong to the loop; an HTTP caller that could write them would
/// fake a source's state, and the next pass would resolve it away.
const RESERVED_SOURCES: &[&str] = &["today", "fleet", "hub"];
const RESERVED_PREFIXES: &[&str] = &["today:", "fleet:", "hub:"];

/// Agents and procedures raise their own rows; only the operator dismisses.
const RAISERS: &[Role] = &[Role::Operator, Role::Agent, Role::Procedure];

const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 16 * 1024;
pub const MAX_LIMIT: i64 = 200;
/// Cap for one long-poll request; the client re-polls after this.
pub const MAX_WAIT_SECS: u64 = 55;

/// "Open" = unresolved, unacknowledged, and not snoozed into the future.
const OPEN: &str = "resolved_at is null and read_at is null and (snoozed_until is null or snoozed_until <= now())";

/// One condition a producer wants the operator to see. Producers build these;
/// `post` builds the same shape from an API request after its checks.
#[derive(Debug, Clone)]
pub struct Raise {
    pub source: String,
    pub dedupe_key: String,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub subject_type: Option<String>,
    pub subject_id: Option<String>,
    pub url: Option<String>,
}

/// The contract's row shape plus the derived `open`.
fn select(where_: &str) -> String {
    format!(
        "select to_jsonb(t) from (select
           id, seq, source, dedupe_key, severity, title, body, subject_type, subject_id, url,
           read_at, resolved_at, snoozed_until, created_at, updated_at, ({OPEN}) as open
         from notifications where {where_}) t"
    )
}

/// Serializes notification writers through commit. `nextval` is not
/// transactional, so without this a writer could allocate seq N, another commit
/// N+1, and a reader advance its cursor past N before N commits. Under the lock
/// every snapshot sees a prefix of the allocated seqs.
// ponytail: one global lock; writes are a handful per minute. Finer locking if that changes.
async fn lock_writers(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> DResult<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtext('hub.notifications'))").execute(&mut **tx).await?;
    Ok(())
}

/// The only write producers use. `true` when a row changed, i.e. when a new or
/// reopened condition was announced.
pub async fn raise(hub: &Hub, raise: &Raise) -> DResult<bool> {
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let changed = upsert(&mut *tx, &hub.config.company_id, raise).await?;
    tx.commit().await?;
    if changed {
        hub.notified.notify_waiters();
    }
    Ok(changed)
}

async fn upsert<'e, E>(db: E, company_id: &str, raise: &Raise) -> DResult<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "insert into notifications (id, company_id, source, dedupe_key, severity, title, body, subject_type, subject_id, url)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         on conflict (company_id, dedupe_key) do update set
           severity = excluded.severity, title = excluded.title, body = excluded.body,
           subject_type = excluded.subject_type, subject_id = excluded.subject_id, url = excluded.url,
           read_at = null, resolved_at = null, seq = nextval('notifications_seq'), updated_at = now()
         where notifications.resolved_at is not null
            or (notifications.severity, notifications.title, notifications.body)
               is distinct from (excluded.severity, excluded.title, excluded.body)",
    )
    .bind(Uuid::new_v4())
    .bind(company_id)
    .bind(&raise.source)
    .bind(&raise.dedupe_key)
    .bind(raise.severity.as_str())
    .bind(&raise.title)
    .bind(&raise.body)
    .bind(&raise.subject_type)
    .bind(&raise.subject_id)
    .bind(&raise.url)
    .execute(db)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// One producer pass: upsert the current set for `source`, then resolve every
/// row of that source the pass did not mention. Returns how many rows changed.
pub async fn sync(hub: &Hub, source: &str, current: Vec<Raise>) -> DResult<usize> {
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let mut changed = 0;
    for raise in &current {
        changed += usize::from(upsert(&mut *tx, &hub.config.company_id, raise).await?);
    }
    let keys: Vec<&str> = current.iter().map(|r| r.dedupe_key.as_str()).collect();
    changed += sqlx::query(
        "update notifications
         set resolved_at = now(), seq = nextval('notifications_seq'), updated_at = now()
         where company_id = $1 and source = $2 and resolved_at is null and dedupe_key <> all($3)",
    )
    .bind(&hub.config.company_id)
    .bind(source)
    .bind(&keys)
    .execute(&mut *tx)
    .await?
    .rows_affected() as usize;
    tx.commit().await?;
    if changed > 0 {
        hub.notified.notify_waiters();
    }
    Ok(changed)
}

/// Without `after`: a snapshot of the newest open (or all) rows, and `cursor` is
/// that snapshot's high-water mark. With `after`: the feed. It takes the oldest
/// `limit` changes past the cursor and advances only through them, so a client
/// that echoes `cursor` never skips a change it did not receive; `state` then
/// only filters what is shown from that window. Items are newest first either
/// way, and the counts always describe *all* open rows.
pub async fn page(hub: &Hub, actor: &Actor, after: Option<i64>, all: bool, limit: i64) -> DResult<NotificationPage> {
    actor.require(crate::ANY_READER)?;
    let limit = limit.clamp(1, MAX_LIMIT);
    // One snapshot for items, cursor and counts.
    let mut tx = hub.db.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    let company = &hub.config.company_id;
    let (items, cursor) = match after {
        None => {
            let rows: Vec<Value> = sqlx::query_scalar(&select(&format!("company_id = $1 and ($2 or ({OPEN})) order by seq desc limit $3")))
                .bind(company)
                .bind(all)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await?;
            let mut items: Vec<Notification> = decode(rows)?;
            items.sort_by_key(|n| std::cmp::Reverse(n.seq));
            let cursor: i64 = sqlx::query_scalar("select coalesce(max(seq), 0) from notifications where company_id = $1")
                .bind(company)
                .fetch_one(&mut *tx)
                .await?;
            (items, cursor)
        }
        Some(after) => {
            let rows: Vec<Value> = sqlx::query_scalar(&select("company_id = $1 and seq > $2 order by seq limit $3"))
                .bind(company)
                .bind(after)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await?;
            let window: Vec<Notification> = decode(rows)?;
            let cursor = window.iter().map(|n| n.seq).max().unwrap_or(after);
            let mut items: Vec<Notification> = window.into_iter().filter(|n| all || n.open).collect();
            items.sort_by_key(|n| std::cmp::Reverse(n.seq));
            (items, cursor)
        }
    };
    let (open, critical): (i64, i64) = sqlx::query_as(&format!(
        "select
           (select count(*) from notifications where company_id = $1 and ({OPEN})),
           (select count(*) from notifications where company_id = $1 and ({OPEN}) and severity = 'critical')"
    ))
    .bind(company)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(NotificationPage { items, cursor, open, critical })
}

/// Operator-only. Acknowledges the given ids, or every open row. One audit row
/// per request: the operator dismissed a batch, not one row at a time.
pub async fn ack(hub: &Hub, actor: &Actor, input: AckNotificationsIn) -> DResult<NotificationPage> {
    actor.require(OPERATOR)?;
    if !input.all && input.ids.is_empty() {
        return Err(invalid("nothing_selected", "pass ids or all"));
    }
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    // `all` means every *open* row: a row snoozed into the future stays unread
    // so it resurfaces when the snooze lapses.
    let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
        "update notifications
         set read_at = now(), seq = nextval('notifications_seq'), updated_at = now()
         where company_id = $1 and read_at is null and (($2 and ({OPEN})) or id = any($3))
         returning id"
    ))
    .bind(&hub.config.company_id)
    .bind(input.all)
    .bind(&input.ids)
    .fetch_all(&mut *tx)
    .await?;
    if !ids.is_empty() {
        let subject = if ids.len() == 1 { ids[0].to_string() } else { "*".to_string() };
        audit(&mut tx, actor, "notification.acked", "notification", subject, json!({ "all": input.all, "ids": ids })).await?;
    }
    tx.commit().await?;
    if !ids.is_empty() {
        hub.notified.notify_waiters();
    }
    page(hub, actor, None, false, MAX_LIMIT).await
}

/// Operator-only. Hides the row until `until`; the producer loop announces a
/// lapsed snooze on its next pass.
pub async fn snooze(hub: &Hub, actor: &Actor, id: Uuid, input: SnoozeNotificationIn) -> DResult<Notification> {
    actor.require(OPERATOR)?;
    if input.until <= Utc::now() {
        return Err(invalid("past_until", "snooze until must be in the future"));
    }
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let changed = sqlx::query(
        "update notifications
         set snoozed_until = $3, seq = nextval('notifications_seq'), updated_at = now()
         where company_id = $1 and id = $2",
    )
    .bind(&hub.config.company_id)
    .bind(id)
    .bind(input.until)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(DomainError::NotFound("notification"));
    }
    audit(&mut tx, actor, "notification.snoozed", "notification", id, json!({ "until": input.until })).await?;
    let row: Value = sqlx::query_scalar(&select("id = $1")).bind(id).fetch_one(&mut *tx).await?;
    let row = decode_one(row)?;
    tx.commit().await?;
    hub.notified.notify_waiters();
    Ok(row)
}

/// `POST /v1/notifications`: an agent, a procedure or the operator raises one
/// row. Reserved keys are rejected because the producer loop owns them.
pub async fn post(hub: &Hub, actor: &Actor, input: RaiseNotificationIn) -> DResult<Notification> {
    actor.require(RAISERS)?;
    let source = input.source.as_deref().unwrap_or("agent").trim().to_string();
    if source.is_empty() {
        return Err(invalid("invalid_source", "source must not be empty"));
    }
    if RESERVED_SOURCES.contains(&source.as_str()) || RESERVED_PREFIXES.iter().any(|p| input.dedupe_key.starts_with(p)) {
        return Err(invalid("reserved_key", "the today, fleet and hub sources and their key prefixes belong to the producer loop"));
    }
    if !valid_key(&input.dedupe_key) {
        return Err(invalid("invalid_dedupe_key", "dedupe_key must match ^[a-z0-9][a-z0-9:._-]{0,127}$"));
    }
    if input.title.trim().is_empty() || input.title.chars().count() > MAX_TITLE {
        return Err(invalid("invalid_title", format!("title must be 1..={MAX_TITLE} characters")));
    }
    if input.body.len() > MAX_BODY {
        return Err(invalid("body_too_large", format!("body must be at most {MAX_BODY} bytes")));
    }
    let key = input.dedupe_key;
    let entry = Raise {
        source,
        dedupe_key: key.clone(),
        severity: input.severity,
        title: input.title,
        body: input.body,
        subject_type: input.subject_type,
        subject_id: input.subject_id,
        url: input.url,
    };
    raise(hub, &entry).await?;
    let row: Option<Value> = sqlx::query_scalar(&select("company_id = $1 and dedupe_key = $2"))
        .bind(&hub.config.company_id)
        .bind(&key)
        .fetch_optional(&hub.db)
        .await?;
    decode_one(row.ok_or(DomainError::NotFound("notification"))?)
}

/// The database check constraint, re-checked here so a bad key is a 422 and not
/// a 500 from the insert.
fn valid_key(key: &str) -> bool {
    let mut bytes = key.as_bytes().iter();
    let Some(first) = bytes.next() else { return false };
    key.len() <= 128
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(*b, b':' | b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_keys_follow_the_database_constraint() {
        for ok in ["a", "agent:test:nightly", "today:renewal:abc:due", "fleet-host_1.v2"] {
            assert!(valid_key(ok), "{ok} is a valid key");
        }
        assert!(valid_key(&format!("a{}", "b".repeat(127))), "128 characters is allowed");
        for bad in ["", ":agent", "Agent:test", "agent test", "-agent", "agent/test"] {
            assert!(!valid_key(bad), "{bad:?} is not a valid key");
        }
        assert!(!valid_key(&format!("a{}", "b".repeat(128))), "129 characters is too long");
    }
}
