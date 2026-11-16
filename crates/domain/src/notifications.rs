//! Notifications: one row per condition, level-triggered by a producer and
//! owned by the operator afterwards.
//!
//! A producer's only write is the upsert: a condition that is still true and
//! unchanged touches nothing (no popup, and an acknowledgement survives), while
//! a changed or returning one reopens and bumps `seq`. `sync` closes the set by
//! resolving the rows a source no longer reports. `seq` is the long-poll cursor,
//! so every change here wakes the waiters after its transaction commits.

use chrono::{DateTime, Utc};
use contracts::fleet::{FleetStatus, Health, NightlyOutcome};
use contracts::{
    AckNotificationsIn, Notification, NotificationPage, RaiseNotificationIn, Role, Severity, SnoozeNotificationIn, SyncNotificationsIn,
    Today, TodayItem,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, audit, decode, decode_one, invalid};

#[cfg(test)]
#[test]
fn secret_raises_follow_the_contract() {
    let now = DateTime::parse_from_rfc3339("2026-11-16T12:00:00Z").unwrap().with_timezone(&Utc);
    let mut value = serde_json::json!({"schemaVersion":1,"generatedAt":"2026-11-16T12:00:00Z","secrets":[]});
    for (id, expiry) in [
        ("expired", json!({"type":"at","date":"2026-11-16"})),
        ("warning", json!({"type":"at","date":"2026-12-16"})),
        ("far", json!({"type":"at","date":"2026-12-17"})),
        ("never", json!({"type":"never"})),
        ("unknown", json!({"type":"unknown"})),
    ] {
        value["secrets"].as_array_mut().unwrap().push(json!({"id":id,"tree":"home","scope":"can","category":"oauth","project":null,"kind":"external","custodian":"can","expiry":expiry,"rotatedAt":null,"rotation":{"method":"replace","command":"rotate","note":""},"consumers":[]}));
    }
    let mut inventory: contracts::SecretInventory = serde_json::from_value(value).unwrap();
    let raises = secret_raises(&inventory, now, &Default::default()).unwrap();
    assert_eq!(
        raises.iter().map(|(_, r)| r.dedupe_key.as_str()).collect::<Vec<_>>(),
        vec!["secrets:expired:expired", "secrets:unknown:unknown", "secrets:warning:expiring"]
    );
    assert!(!secret_inventory_stale(&inventory, now));
    assert!(secret_inventory_stale(&inventory, now + chrono::Duration::hours(26)));
    inventory.generated_at = (now + chrono::Duration::seconds(61)).to_rfc3339();
    assert!(secret_inventory_stale(&inventory, now));
    inventory.generated_at = "invalid".into();
    assert!(secret_inventory_stale(&inventory, now));
}

#[cfg(test)]
#[test]
fn secret_custodian_requires_a_member_principal() {
    let now = Utc::now();
    let value = json!({"schemaVersion":1,"generatedAt":now.to_rfc3339(),"secrets":[
        {"id":"private","tree":"home","scope":"dejana","category":"oauth","project":null,"kind":"external","custodian":"dejana","expiry":{"type":"unknown"},"rotatedAt":null,"rotation":{"method":"replace","command":"replace","note":""},"consumers":[]},
        {"id":"operator","tree":"home","scope":"can","category":"oauth","project":null,"kind":"external","custodian":"can","expiry":{"type":"unknown"},"rotatedAt":null,"rotation":{"method":"replace","command":"replace","note":""},"consumers":[]}
    ]});
    let inventory = serde_json::from_value(value).unwrap();
    let raises = secret_raises(&inventory, now, &["dejana".to_string()].into_iter().collect()).unwrap();
    assert!(raises.iter().any(|(owner, r)| owner.as_deref() == Some("dejana") && r.dedupe_key == "member:dejana:secrets:private:unknown"));
    assert!(raises.iter().any(|(owner, r)| owner.is_none() && r.dedupe_key == "secrets:operator:unknown"));
    assert!(secret_raises(&inventory, now, &Default::default()).unwrap().iter().all(|(owner, _)| owner.is_none()));
}

/// Producer keys belong to the loop; an HTTP caller that could write them would
/// fake a source's state, and the next pass would resolve it away.
const RESERVED_SOURCES: &[&str] = &["today", "fleet", "hub", "secrets"];
const RESERVED_PREFIXES: &[&str] = &["today:", "fleet:", "hub:", "secrets:"];

/// Agents and procedures raise their own rows; only the operator dismisses.
const RAISERS: &[Role] = &[Role::Operator, Role::Agent, Role::Procedure, Role::Member];
const READERS: &[Role] = &[Role::Operator, Role::Agent, Role::Member];
const HUMAN: &[Role] = &[Role::Operator, Role::Member];

const MAX_TITLE: usize = 200;
const MAX_BODY: usize = 16 * 1024;
/// Sync copies the source into every item, so it is bounded before that.
const MAX_SOURCE: usize = 64;
pub const MAX_LIMIT: i64 = 200;
/// Cap for one long-poll request; the client re-polls after this.
pub const MAX_WAIT_SECS: u64 = 55;

/// "Open" = unresolved, unacknowledged, and not snoozed into the future.
const OPEN: &str = "resolved_at is null and read_at is null and (snoozed_until is null or snoozed_until <= now())";

/// One condition a producer wants the operator to see. Producers build these;
/// `post` builds the same shape from an API request after its checks.
#[derive(Debug, Clone, PartialEq)]
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
    let changed = upsert(&mut *tx, &hub.config.company_id, None, raise).await?;
    tx.commit().await?;
    if changed {
        hub.notified.notify_waiters();
    }
    Ok(changed)
}

async fn upsert<'e, E>(db: E, company_id: &str, owner: Option<&str>, raise: &Raise) -> DResult<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "insert into notifications (id, company_id, source, dedupe_key, severity, title, body, subject_type, subject_id, url, owner)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         on conflict (company_id, dedupe_key) do update set
           severity = excluded.severity, title = excluded.title, body = excluded.body,
           subject_type = excluded.subject_type, subject_id = excluded.subject_id, url = excluded.url,
           read_at = null, resolved_at = null, seq = nextval('notifications_seq'), updated_at = now()
          where notifications.owner is not distinct from excluded.owner and
            (notifications.resolved_at is not null
             or (notifications.severity, notifications.title, notifications.body)
                is distinct from (excluded.severity, excluded.title, excluded.body))",
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
    .bind(owner)
    .execute(db)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// One producer pass: upsert the current set for `source`, then resolve every
/// row of that source the pass did not mention. Returns how many rows changed.
pub async fn sync(hub: &Hub, source: &str, current: Vec<Raise>) -> DResult<usize> {
    sync_owned(hub, None, source, current).await
}

async fn sync_owned(hub: &Hub, owner: Option<&str>, source: &str, current: Vec<Raise>) -> DResult<usize> {
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let mut changed = 0;
    for raise in &current {
        let collision: bool = sqlx::query_scalar(
            "select exists(select 1 from notifications where company_id = $1 and dedupe_key = $2
                and (owner is distinct from $3 or ($3 is not null and $4 = 'member:' || $3 || ':renewals' and source <> $4)))",
        )
        .bind(&hub.config.company_id)
        .bind(&raise.dedupe_key)
        .bind(owner)
        .bind(source)
        .fetch_one(&mut *tx)
        .await?;
        if collision {
            return Err(invalid("invalid_dedupe_key", "key belongs to another notification owner or source"));
        }
        changed += usize::from(upsert(&mut *tx, &hub.config.company_id, owner, raise).await?);
    }
    let keys: Vec<&str> = current.iter().map(|r| r.dedupe_key.as_str()).collect();
    changed += sqlx::query(
        "update notifications
         set resolved_at = now(), seq = nextval('notifications_seq'), updated_at = now()
         where company_id = $1 and source = $2 and resolved_at is null and dedupe_key <> all($3) and owner is not distinct from $4",
    )
    .bind(&hub.config.company_id)
    .bind(source)
    .bind(&keys)
    .bind(owner)
    .execute(&mut *tx)
    .await?
    .rows_affected() as usize;
    tx.commit().await?;
    if changed > 0 {
        hub.notified.notify_waiters();
    }
    Ok(changed)
}

// ── Producers ───────────────────────────────────────────────────────────────

/// A snapshot older than this must never look healthy; the same rule the
/// desktop fleet page applies.
const FLEET_STALE_AFTER_SECS: i64 = 15 * 60;
/// Clock difference tolerated between the snapshot's writer and the hub.
const FLEET_CLOCK_SKEW_SECS: i64 = 60;
// ponytail: fixed bounds; make configurable if a pipeline legitimately runs longer.
const STALLED_RUNNING_HOURS: i32 = 2;
const STALLED_PENDING_HOURS: i32 = 6;

// ponytail: fixed warning window; make configurable if different policies are needed.
const SECRET_WARN_DAYS: i64 = 30;

pub fn secret_state(secret: &contracts::Secret, now: DateTime<Utc>) -> &'static str {
    match secret.expiry {
        contracts::SecretExpiry::Never => "never",
        contracts::SecretExpiry::Unknown => "unknown",
        contracts::SecretExpiry::At { date } if date <= now.date_naive() => "expired",
        contracts::SecretExpiry::At { date } if date.signed_duration_since(now.date_naive()).num_days() <= SECRET_WARN_DAYS => "expiring",
        contracts::SecretExpiry::At { .. } => "ok",
    }
}

pub fn secret_inventory_stale(inventory: &contracts::SecretInventory, now: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(&inventory.generated_at) {
        Ok(generated) => {
            let age = now.signed_duration_since(generated.with_timezone(&Utc));
            age > chrono::Duration::hours(25) || age < chrono::Duration::seconds(-60)
        }
        Err(_) => true,
    }
}

pub fn secret_raises(
    inventory: &contracts::SecretInventory,
    now: DateTime<Utc>,
    members: &std::collections::BTreeSet<String>,
) -> DResult<Vec<(Option<String>, Raise)>> {
    let mut groups = std::collections::BTreeMap::<Option<String>, Vec<Raise>>::new();
    for secret in &inventory.secrets {
        let state = secret_state(secret, now);
        let severity = match state {
            "expired" => Severity::Critical,
            "expiring" => Severity::Warning,
            "unknown" => Severity::Info,
            _ => continue,
        };
        let date = match &secret.expiry {
            contracts::SecretExpiry::At { date } => date.to_string(),
            _ => "unknown".into(),
        };
        let owner = members.contains(&secret.custodian).then_some(secret.custodian.clone());
        let mut key = key_safe(&format!("secrets:{}:{state}", secret.id));
        if let Some(name) = &owner {
            let prefix = stored_key(Some(name), "s")?;
            key.truncate(128 - (prefix.len() - 1));
        }
        let key = stored_key(owner.as_deref(), &key)?;
        groups.entry(owner).or_default().push(Raise {
            source: "secrets".into(),
            dedupe_key: key,
            severity,
            title: format!("Secret {}: {state}", secret.id),
            body: format!(
                "{}; date: {date}; custodian: {}; rotation: {} — {}",
                secret.id, secret.custodian, secret.rotation.method, secret.rotation.command
            ),
            subject_type: None,
            subject_id: None,
            url: None,
        });
    }
    Ok(groups.into_iter().flat_map(|(owner, raises)| merge_same_key(raises).into_iter().map(move |raise| (owner.clone(), raise))).collect())
}

pub async fn sync_secrets(hub: &Hub, current: Vec<(Option<String>, Raise)>) -> DResult<usize> {
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let mut changed = 0;
    for (owner, raise) in &current {
        let collision: bool = sqlx::query_scalar("select exists(select 1 from notifications where company_id = $1 and dedupe_key = $2 and (owner is distinct from $3 or source <> 'secrets'))")
            .bind(&hub.config.company_id).bind(&raise.dedupe_key).bind(owner).fetch_one(&mut *tx).await?;
        if collision {
            return Err(invalid("invalid_dedupe_key", "key belongs to another notification owner or source"));
        }
        changed += usize::from(upsert(&mut *tx, &hub.config.company_id, owner.as_deref(), raise).await?);
    }
    let keys: Vec<&str> = current.iter().map(|(_, r)| r.dedupe_key.as_str()).collect();
    changed += sqlx::query("update notifications set resolved_at = now(), seq = nextval('notifications_seq'), updated_at = now() where company_id = $1 and source = 'secrets' and resolved_at is null and dedupe_key <> all($2)")
        .bind(&hub.config.company_id).bind(&keys).execute(&mut *tx).await?.rows_affected() as usize;
    tx.commit().await?;
    if changed > 0 {
        hub.notified.notify_waiters();
    }
    Ok(changed)
}

/// Include every stalled generation, not the capped informational Today list.
pub async fn today_with_stalled(hub: &Hub, today: &Today) -> DResult<Vec<Raise>> {
    // Times come from `state_since` (migration 0008), not `updated_at`, which
    // every progress poll bumps. A `running` generation is judged by its open
    // attempts: one really executing gets the running bound from when it
    // started, while attempts still submitted or queued (a pipeline's
    // `Pending`) get the pending bound from when they were queued.
    let rows: Vec<(Uuid, Uuid, String, String, DateTime<Utc>)> = sqlx::query_as(
        "select g.id, c.id, c.title,
                case when g.state = 'running' then a.label else g.state end,
                case when g.state = 'running' then a.since else g.state_since end
         from generation_requests g
         join content_items c on c.id = g.content_item_id
         left join lateral (
             select case when bool_or(x.state = 'running') then 'running' else 'queued' end as label,
                    coalesce(min(x.state_since) filter (where x.state = 'running'), min(x.state_since)) as since
             from execution_attempts x
             where x.generation_request_id = g.id and x.state in ('submitted', 'queued', 'running')
             having count(*) > 0
         ) a on true
         where (g.state in ('requested', 'dispatched') and g.state_since < now() - make_interval(hours => $2))
            or (g.state = 'running' and a.label = 'running' and a.since < now() - make_interval(hours => $1))
            or (g.state = 'running' and a.label = 'queued' and a.since < now() - make_interval(hours => $2))",
    )
    .bind(STALLED_RUNNING_HOURS)
    .bind(STALLED_PENDING_HOURS)
    .fetch_all(&hub.db)
    .await?;
    let mut raises = today_raises(today);
    raises.extend(rows.into_iter().map(|(id, content_id, title, state, since)| Raise {
        source: "today".into(),
        dedupe_key: format!("today:stalled:{id}"),
        severity: Severity::Warning,
        title: format!("{title}: {state}"),
        body: format!("{state} since {}", since.to_rfc3339()),
        subject_type: Some("content".into()),
        subject_id: Some(content_id.to_string()),
        url: None,
    }));
    Ok(merge_same_key(raises))
}

/// One raise per Today row the operator must act on: reviews, failed
/// operations, overdue interactions, renewals and opportunities.
/// `due_releases` and `active_production` are informational and never notify.
pub fn today_raises(today: &Today) -> Vec<Raise> {
    let mut raises = Vec::new();
    let mut push = |severity: Severity, dedupe_key: String, item: &TodayItem| {
        raises.push(Raise {
            source: "today".into(),
            dedupe_key,
            severity,
            title: item.title.clone(),
            body: item.status.clone(),
            subject_type: Some(item.subject_type.clone()),
            subject_id: Some(item.subject_id.to_string()),
            url: None,
        });
    };
    for item in &today.pending_reviews {
        push(Severity::Info, format!("today:review:{}:{}", item.kind, item.subject_id), item);
    }
    for item in &today.failed_operations {
        let severity = if item.status == "outcome_unknown" { Severity::Warning } else { Severity::Critical };
        push(severity, format!("today:failed:{}:{}:{}", item.kind, item.subject_id, item.status), item);
    }
    for item in &today.overdue_interactions {
        push(Severity::Warning, format!("today:overdue:{}", item.subject_id), item);
    }
    for item in &today.renewals {
        // `never` is a console fact, not a to-do; it must not reach Today, but
        // a stray one still never notifies.
        if let Some(severity) = renewal_severity(&item.status) {
            push(severity, format!("today:renewal:{}:{}", item.subject_id, item.status), item);
        }
    }
    for item in &today.opportunities {
        push(Severity::Info, format!("today:opportunity:{}:{}", item.subject_id, item.status), item);
    }
    merge_same_key(raises)
}

fn renewal_severity(status: &str) -> Option<Severity> {
    match status {
        "ok" | "never" => None,
        "overdue" | "invalid" => Some(Severity::Critical),
        "due" | "unverifiable" => Some(Severity::Warning),
        _ => Some(Severity::Info),
    }
}

pub async fn sync_member_renewals(hub: &Hub) -> DResult<()> {
    let owners: Vec<String> = sqlx::query_scalar(
        "select owner from renewals where company_id = $1 and owner is not null
         union select owner from notifications where company_id = $1 and owner is not null
           and source = 'member:' || owner || ':renewals' and resolved_at is null",
    )
    .bind(&hub.config.company_id)
    .fetch_all(&hub.db)
    .await?;
    for owner in owners {
        let source = format!("member:{owner}:renewals");
        let mut raises = Vec::new();
        for row in crate::renewals::all(hub, Some(&owner)).await? {
            if let Some(severity) = renewal_severity(&row.status) {
                raises.push(Raise {
                    source: source.clone(),
                    dedupe_key: stored_key(Some(&owner), &format!("renewal:{}:{}", row.key, row.status))?,
                    severity,
                    title: row.title,
                    body: row.status,
                    subject_type: Some("renewal".into()),
                    subject_id: Some(row.id.to_string()),
                    url: None,
                });
            }
        }
        sync_owned(hub, Some(&owner), &source, raises).await?;
    }
    Ok(())
}

/// Producer keys embed external text (host names, alert keys, timestamps such
/// as `2026-11-15T02:00:00.7+00:00`). One key that breaks the table's check
/// would roll back a whole `sync`, so every producer key is folded into the
/// allowed alphabet: lowercase, anything else becomes `-`, at most 128 bytes.
fn key_safe(key: &str) -> String {
    let mut out: String = key
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '.' | '_' | '-') { c } else { '-' })
        .collect();
    out.truncate(128);
    out
}

/// Several Today rows can share one key (two failed destinations of the same
/// content). Syncing them separately would overwrite one row every pass and
/// clear its acknowledgement, so rows with the same key become one raise with
/// a deterministic title and body.
fn merge_same_key(raises: Vec<Raise>) -> Vec<Raise> {
    let mut groups: std::collections::BTreeMap<String, Vec<Raise>> = std::collections::BTreeMap::new();
    for raise in raises {
        groups.entry(key_safe(&raise.dedupe_key)).or_default().push(raise);
    }
    groups
        .into_iter()
        .map(|(key, mut group)| {
            group.sort_by(|a, b| (&a.title, &a.body).cmp(&(&b.title, &b.body)));
            let mut first = group.remove(0);
            first.dedupe_key = key;
            if !group.is_empty() {
                let mut lines = vec![format!("{}: {}", first.title, first.body)];
                lines.extend(group.iter().map(|r| format!("{}: {}", r.title, r.body)));
                first.title = format!("{} (+{} more)", first.title, group.len());
                first.body = lines.join("\n");
            }
            first
        })
        .collect()
}

/// One raise per operator concern in a snapshot. A host is named only when its
/// health is not healthy: every host is outdated most of the day, so revision
/// drift alone never notifies. A snapshot older than 15 minutes, or one whose
/// age is unknowable, is itself `fleet:stale`.
pub fn fleet_raises(status: &FleetStatus, now: DateTime<Utc>) -> Vec<Raise> {
    let mut raises = Vec::new();
    let mut push = |dedupe_key: String, title: String, body: String| {
        raises.push(Raise {
            source: "fleet".into(),
            dedupe_key,
            severity: Severity::Warning,
            title,
            body,
            subject_type: None,
            subject_id: None,
            url: None,
        });
    };
    for host in &status.hosts {
        if host.health == Health::Healthy {
            continue;
        }
        let health = match host.health {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Unreachable => "unreachable",
        };
        let body = if host.failures.is_empty() { health.to_string() } else { host.failures.join("; ") };
        push(format!("fleet:host:{}", host.name), format!("Host {} is {health}", host.name), body);
    }
    if let Some(error) = &status.monitoring.error {
        push("fleet:monitoring".into(), "Monitoring unreadable".into(), error.clone());
    }
    for alert in &status.monitoring.alerts {
        let body = if alert.detail.is_empty() { alert.group.clone() } else { alert.detail.join("; ") };
        push(format!("fleet:alert:{}", alert.key), format!("Alert: {}", alert.name), body);
    }
    for op in &status.pending_ops {
        let body = if op.reason.is_empty() {
            format!("{} — approve with `canix host rops`", op.host)
        } else {
            format!("{} on {} — approve with `canix host rops`", op.reason, op.host)
        };
        push(format!("fleet:op:{}", op.id), format!("Approval waiting: {}", op.title), body);
    }
    if let Some(nightly) = &status.nightly
        && nightly.failed()
    {
        let body = nightly.error.clone().unwrap_or_else(|| {
            let failed: Vec<&str> = nightly.hosts.iter().filter(|h| h.outcome == NightlyOutcome::Failed).map(|h| h.host.as_str()).collect();
            if failed.is_empty() { "the run failed".to_string() } else { format!("failed on {}", failed.join(", ")) }
        });
        push(format!("fleet:nightly:{}", nightly.started_at), "Nightly update failed".into(), body);
    }
    match snapshot_age_secs(status, now) {
        // The body names the snapshot, not its age: an age changes every pass,
        // which would reopen an acknowledged outage each minute.
        Some(age) if age > FLEET_STALE_AFTER_SECS => push(
            "fleet:stale".into(),
            "Fleet snapshot is stale".into(),
            format!("last snapshot generated at {}; fleet status is not evidence the fleet is healthy", status.generated_at),
        ),
        Some(_) => {}
        None => push("fleet:stale".into(), "Fleet snapshot is stale".into(), "generatedAt is not a time this hub can compare".into()),
    }
    merge_same_key(raises)
}

/// The single raise a pass uses when the configured snapshot cannot be read.
pub fn fleet_unreadable(error: &str) -> Raise {
    Raise {
        source: "fleet".into(),
        dedupe_key: "fleet:stale".into(),
        severity: Severity::Warning,
        title: "Fleet snapshot is unreadable".into(),
        body: error.to_string(),
        subject_type: None,
        subject_id: None,
        url: None,
    }
}

/// Age of the snapshot in seconds, or `None` when it cannot be trusted: an
/// unparsable `generatedAt`, or one further in the future than clock skew
/// explains.
fn snapshot_age_secs(status: &FleetStatus, now: DateTime<Utc>) -> Option<i64> {
    let then = DateTime::parse_from_rfc3339(&status.generated_at).ok()?.with_timezone(&Utc);
    let age = now.signed_duration_since(then).num_seconds();
    (age >= -FLEET_CLOCK_SKEW_SECS).then_some(age)
}

/// `hub:summary-stale` while no `summary` row has been written for
/// `HUB_SUMMARY_STALE_HOURS`. Unset means the rule is off: the source syncs an
/// empty set, so any raise it left resolves.
pub async fn summary_raises(hub: &Hub, stale_hours: Option<u32>) -> DResult<Vec<Raise>> {
    let Some(hours) = stale_hours else { return Ok(Vec::new()) };
    let fresh: bool = sqlx::query_scalar(
        "select exists(
           select 1 from notifications
           where company_id = $1 and source = 'summary' and updated_at > now() - make_interval(hours => $2))",
    )
    .bind(&hub.config.company_id)
    .bind(hours.min(i32::MAX as u32) as i32)
    .fetch_one(&hub.db)
    .await?;
    Ok(if fresh {
        Vec::new()
    } else {
        vec![Raise {
            source: "hub".into(),
            dedupe_key: "hub:summary-stale".into(),
            severity: Severity::Warning,
            title: "No daily summary".into(),
            body: format!("no summary in the last {hours} h: the scheduled task has not run. Run it now."),
            subject_type: None,
            subject_id: None,
            url: None,
        }]
    })
}

/// Clear lapsed snoozes and announce them. Holds the writer lock because the
/// update bumps `seq`: `nextval` is not transactional, so without it this writer
/// could commit a lower seq after a concurrent one and a reader would advance
/// its cursor past a change it never received.
pub async fn expire_snoozes(hub: &Hub) -> DResult<usize> {
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let changed = sqlx::query(
        "update notifications
         set snoozed_until = null, seq = nextval('notifications_seq'), updated_at = now()
         where company_id = $1 and snoozed_until <= now()",
    )
    .bind(&hub.config.company_id)
    .execute(&mut *tx)
    .await?
    .rows_affected() as usize;
    tx.commit().await?;
    if changed > 0 {
        hub.notified.notify_waiters();
    }
    Ok(changed)
}

/// Drop rows the operator both acknowledged and saw resolve, 30 days on. A
/// delete allocates no seq, so it needs no writer lock.
pub async fn prune(hub: &Hub) -> DResult<u64> {
    Ok(sqlx::query(
        "delete from notifications
         where company_id = $1 and resolved_at < now() - interval '30 days' and read_at is not null",
    )
    .bind(&hub.config.company_id)
    .execute(&hub.db)
    .await?
    .rows_affected())
}

/// Without `after`: a snapshot of the newest open (or all) rows, and `cursor` is
/// that snapshot's high-water mark. With `after`: the feed. It takes the oldest
/// `limit` changes past the cursor and advances only through them, so a client
/// that echoes `cursor` never skips a change it did not receive; `state` then
/// only filters what is shown from that window. Items are newest first either
/// way, and the counts always describe *all* open rows.
fn view_owner<'a>(actor: &'a Actor, requested: Option<&'a str>) -> DResult<Option<&'a str>> {
    actor.require(READERS)?;
    if requested.is_some() && actor.principal.role != Role::Operator {
        return Err(DomainError::Forbidden("only the operator may select an owner".into()));
    }
    Ok(if actor.principal.role == Role::Member { Some(&actor.principal.name) } else { requested })
}

pub async fn page(hub: &Hub, actor: &Actor, owner: Option<&str>, after: Option<i64>, all: bool, limit: i64) -> DResult<NotificationPage> {
    let owner = view_owner(actor, owner)?;
    let visible =
        if actor.principal.role == Role::Operator { "and (owner is null or source <> 'member:' || owner || ':renewals')" } else { "" };
    let limit = limit.clamp(1, MAX_LIMIT);
    // One snapshot for items, cursor and counts.
    let mut tx = hub.db.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    let company = &hub.config.company_id;
    let (items, cursor) = match after {
        None => {
            let rows: Vec<Value> = sqlx::query_scalar(&select(&format!(
                "company_id = $1 and ($2 or ({OPEN})) and owner is not distinct from $4 {visible} order by seq desc limit $3"
            )))
            .bind(company)
            .bind(all)
            .bind(limit)
            .bind(owner)
            .fetch_all(&mut *tx)
            .await?;
            let mut items: Vec<Notification> = decode(rows)?;
            items.sort_by_key(|n| std::cmp::Reverse(n.seq));
            let cursor: i64 = sqlx::query_scalar(&format!(
                "select coalesce(max(seq), 0) from notifications where company_id = $1 and owner is not distinct from $2 {visible}"
            ))
            .bind(company)
            .bind(owner)
            .fetch_one(&mut *tx)
            .await?;
            (items, cursor)
        }
        Some(after) => {
            let rows: Vec<Value> = sqlx::query_scalar(&select(&format!(
                "company_id = $1 and seq > $2 and owner is not distinct from $4 {visible} order by seq limit $3"
            )))
            .bind(company)
            .bind(after)
            .bind(limit)
            .bind(owner)
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
           (select count(*) from notifications where company_id = $1 and owner is not distinct from $2 {visible} and ({OPEN})),
           (select count(*) from notifications where company_id = $1 and owner is not distinct from $2 {visible} and ({OPEN}) and severity = 'critical')"
    ))
    .bind(company)
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(NotificationPage { items, cursor, open, critical })
}

/// Acknowledges the given ids, or every open row in the selected view.
pub async fn ack(hub: &Hub, actor: &Actor, owner: Option<&str>, input: AckNotificationsIn) -> DResult<NotificationPage> {
    actor.require(HUMAN)?;
    let visible =
        if actor.principal.role == Role::Operator { "and (owner is null or source <> 'member:' || owner || ':renewals')" } else { "" };
    let owner = view_owner(actor, owner)?;
    if !input.all && input.ids.is_empty() {
        return Err(invalid("nothing_selected", "pass ids or all"));
    }
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    if !input.all && !input.ids.is_empty() {
        let found: i64 = sqlx::query_scalar(&format!(
            "select count(distinct id) from notifications where company_id = $1 and owner is not distinct from $2 and id = any($3) {visible}"
        ))
        .bind(&hub.config.company_id)
        .bind(owner)
        .bind(&input.ids)
        .fetch_one(&mut *tx)
        .await?;
        if found as usize != input.ids.iter().collect::<std::collections::HashSet<_>>().len() {
            return Err(DomainError::NotFound("notification"));
        }
    }
    // `all` means every *open* row: a row snoozed into the future stays unread
    // so it resurfaces when the snooze lapses.
    let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
        "update notifications
         set read_at = now(), seq = nextval('notifications_seq'), updated_at = now()
         where company_id = $1 and owner is not distinct from $4 {visible} and read_at is null and (($2 and ({OPEN})) or (not $2 and id = any($3)))
         returning id"
    ))
    .bind(&hub.config.company_id)
    .bind(input.all)
    .bind(&input.ids)
    .bind(owner)
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
    page(hub, actor, if actor.principal.role == Role::Operator { owner } else { None }, None, false, MAX_LIMIT).await
}

/// Hides the row until `until`; the producer loop announces a
/// lapsed snooze on its next pass.
pub async fn snooze(hub: &Hub, actor: &Actor, owner: Option<&str>, id: Uuid, input: SnoozeNotificationIn) -> DResult<Notification> {
    actor.require(HUMAN)?;
    let visible =
        if actor.principal.role == Role::Operator { "and (owner is null or source <> 'member:' || owner || ':renewals')" } else { "" };
    let owner = view_owner(actor, owner)?;
    if input.until <= Utc::now() {
        return Err(invalid("past_until", "snooze until must be in the future"));
    }
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let changed = sqlx::query(&format!(
        "update notifications
         set snoozed_until = $3, seq = nextval('notifications_seq'), updated_at = now()
         where company_id = $1 and id = $2 and owner is not distinct from $4 {visible}"
    ))
    .bind(&hub.config.company_id)
    .bind(id)
    .bind(input.until)
    .bind(owner)
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
    let owner = (actor.principal.role == Role::Member).then_some(actor.principal.name.as_str());
    let entry = api_raise(owner, input)?;
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    let changed = upsert(&mut *tx, &hub.config.company_id, owner, &entry).await?;
    let row: Option<Value> = sqlx::query_scalar(&select("company_id = $1 and dedupe_key = $2 and owner is not distinct from $3"))
        .bind(&hub.config.company_id)
        .bind(&entry.dedupe_key)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;
    let row = decode_one(row.ok_or(DomainError::NotFound("notification"))?)?;
    tx.commit().await?;
    if changed {
        hub.notified.notify_waiters();
    }
    Ok(row)
}

fn stored_key(owner: Option<&str>, key: &str) -> DResult<String> {
    if !valid_key(key) || owner.is_some_and(|name| !valid_key(name) || name.contains(':')) {
        return Err(invalid("invalid_dedupe_key", "invalid key or member name"));
    }
    let stored = owner.map_or_else(|| key.to_string(), |name| format!("member:{name}:{key}"));
    if !valid_key(&stored) {
        return Err(invalid("invalid_dedupe_key", "stored dedupe_key exceeds 128 bytes"));
    }
    Ok(stored)
}

pub fn validate_member_renewal_key(owner: &str, key: &str) -> DResult<()> {
    stored_key(Some(owner), &format!("renewal:{key}:unverifiable"))?;
    Ok(())
}

fn api_raise(owner: Option<&str>, input: RaiseNotificationIn) -> DResult<Raise> {
    let source = input.source.as_deref().unwrap_or("agent").trim().to_string();
    if source.is_empty() || source.len() > MAX_SOURCE {
        return Err(invalid("invalid_source", format!("source must be 1..={MAX_SOURCE} bytes")));
    }
    if RESERVED_SOURCES.contains(&source.as_str())
        || (owner.is_some() && (source == "renewals" || input.dedupe_key.starts_with("renewal:")))
        || (owner.is_none() && (source.starts_with("member:") || input.dedupe_key.starts_with("member:")))
        || RESERVED_PREFIXES.iter().any(|p| input.dedupe_key.starts_with(p))
    {
        return Err(invalid("reserved_key", "the today, fleet and hub sources and their key prefixes belong to the producer loop"));
    }
    let key = stored_key(owner, &input.dedupe_key)?;
    if input.title.trim().is_empty() || input.title.chars().count() > MAX_TITLE {
        return Err(invalid("invalid_title", format!("title must be 1..={MAX_TITLE} characters")));
    }
    if input.body.len() > MAX_BODY {
        return Err(invalid("body_too_large", format!("body must be at most {MAX_BODY} bytes")));
    }
    Ok(Raise {
        source: owner.map_or(source.clone(), |name| format!("member:{name}:{source}")),
        dedupe_key: key,
        severity: input.severity,
        title: input.title,
        body: input.body,
        subject_type: input.subject_type,
        subject_id: input.subject_id,
        url: input.url,
    })
}

pub async fn sync_api(hub: &Hub, actor: &Actor, input: SyncNotificationsIn) -> DResult<usize> {
    actor.require(RAISERS)?;
    let owner = (actor.principal.role == Role::Member).then_some(actor.principal.name.as_str());
    let source = input.source.trim();
    if source.is_empty() || source.len() > MAX_SOURCE {
        return Err(invalid("invalid_source", format!("source must be 1..={MAX_SOURCE} bytes")));
    }
    if RESERVED_SOURCES.contains(&source) || (owner.is_some() && source == "renewals") || (owner.is_none() && source.starts_with("member:"))
    {
        return Err(invalid("reserved_key", "reserved source"));
    }
    let current = input
        .items
        .into_iter()
        .map(|item| {
            api_raise(
                owner,
                RaiseNotificationIn {
                    source: Some(source.to_string()),
                    dedupe_key: item.dedupe_key,
                    severity: item.severity,
                    title: item.title,
                    body: item.body,
                    subject_type: item.subject_type,
                    subject_id: item.subject_id,
                    url: item.url,
                },
            )
        })
        .collect::<DResult<Vec<_>>>()?;
    let stored_source = owner.map_or_else(|| source.to_string(), |name| format!("member:{name}:{source}"));
    sync_owned(hub, owner, &stored_source, current).await
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

    #[test]
    fn member_stored_keys_are_unambiguous_and_bounded() {
        assert_eq!(stored_key(Some("dejana"), "unit:one").unwrap(), "member:dejana:unit:one");
        assert!(stored_key(Some("a:b"), "x").is_err());
        assert!(stored_key(Some("dejana"), &"x".repeat(115)).is_err());
        assert!(stored_key(Some("dejana"), &"x".repeat(114)).is_ok());
    }

    /// The fleet snapshot fixture the desktop fleet page uses, with murph
    /// healthy on an old revision: outdated-but-healthy must not notify.
    const FLEET_SNAPSHOT: &str = r#"{
      "schemaVersion": 1,
      "generatedAt": "2026-11-10T03:10:00Z",
      "targetRevision": "new",
      "hosts": [
        {"name": "atlas", "health": "healthy", "revision": "new", "checkedAt": "2026-11-10T03:09:00Z"},
        {"name": "nomad", "health": "degraded", "revision": "old", "failures": ["forgejo: inactive"], "checkedAt": "2026-11-10T03:09:00Z"},
        {"name": "murph", "health": "healthy", "revision": "old", "checkedAt": "2026-11-10T03:09:00Z"}
      ],
      "monitoring": {
        "error": "relay refused",
        "monitored": 12,
        "alerts": [{"key": "core_forgejo", "name": "forgejo", "group": "core", "since": "2026-11-10T02:00:00Z", "detail": ["[STATUS] == 200"]}]
      },
      "pendingOps": [
        {"id": "op-2", "host": "nomad", "title": "restart forgejo", "reason": "unit is down", "requestedBy": "nightly", "created": "2026-11-10T03:05:00Z"},
        {"id": "op-1", "host": "atlas", "title": "finish activation", "created": "2026-11-10T03:00:00Z"}
      ],
      "nightly": {
        "startedAt": "2026-11-10T02:00:00Z",
        "revision": "new",
        "commit": "0123abc",
        "hosts": [{"host": "nomad", "outcome": "failed", "detail": "health check failed"}]
      }
    }"#;

    fn today_item(kind: &str, subject_type: &str, subject_id: Uuid, status: &str) -> TodayItem {
        TodayItem {
            kind: kind.into(),
            title: format!("{kind} {status}"),
            subject_type: subject_type.into(),
            subject_id,
            status: status.into(),
            at: None,
        }
    }

    #[test]
    fn producers_map_every_bucket_once() {
        let id = Uuid::from_u128;
        let today = Today {
            pending_reviews: vec![today_item("release_review", "content", id(1), "draft")],
            // Informational buckets: present in Today, never notified.
            active_production: vec![today_item("generation", "content", id(9), "running")],
            due_releases: vec![today_item("publication", "content", id(8), "scheduled")],
            failed_operations: vec![
                today_item("publication", "content", id(2), "outcome_unknown"),
                today_item("generation", "content", id(3), "failed"),
            ],
            overdue_interactions: vec![today_item("follow_up", "relationship", id(4), "agent")],
            opportunities: vec![today_item("opportunity", "opportunity", id(5), "shortlisted")],
            renewals: vec![
                today_item("renewal", "renewal", id(6), "overdue"),
                today_item("renewal", "renewal", id(7), "due"),
                today_item("renewal", "renewal", id(10), "unknown"),
                today_item("renewal", "renewal", id(11), "unverifiable"),
                today_item("renewal", "renewal", id(12), "invalid"),
                // `never` never reaches Today; if one does, it still notifies
                // about nothing.
                today_item("renewal", "renewal", id(13), "never"),
            ],
        };
        let status: FleetStatus = serde_json::from_str(FLEET_SNAPSHOT).unwrap();
        let generated = DateTime::parse_from_rfc3339("2026-11-10T03:10:00Z").unwrap().with_timezone(&Utc);

        let from_today = today_raises(&today);
        let from_fleet = fleet_raises(&status, generated + chrono::Duration::minutes(5));
        let mut keys: Vec<&str> = from_today.iter().chain(&from_fleet).map(|r| r.dedupe_key.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "fleet:alert:core_forgejo",
                "fleet:host:nomad",
                "fleet:monitoring",
                // Folded into the key alphabet: lowercase, no `+`.
                "fleet:nightly:2026-11-10t02:00:00z",
                "fleet:op:op-1",
                "fleet:op:op-2",
                "today:failed:generation:00000000-0000-0000-0000-000000000003:failed",
                "today:failed:publication:00000000-0000-0000-0000-000000000002:outcome_unknown",
                "today:opportunity:00000000-0000-0000-0000-000000000005:shortlisted",
                "today:overdue:00000000-0000-0000-0000-000000000004",
                "today:renewal:00000000-0000-0000-0000-000000000006:overdue",
                "today:renewal:00000000-0000-0000-0000-000000000007:due",
                "today:renewal:00000000-0000-0000-0000-00000000000a:unknown",
                "today:renewal:00000000-0000-0000-0000-00000000000b:unverifiable",
                "today:renewal:00000000-0000-0000-0000-00000000000c:invalid",
                "today:review:release_review:00000000-0000-0000-0000-000000000001",
            ]
        );
        for quiet in ["000000000008", "000000000009", "00000000000d"] {
            assert!(!keys.iter().any(|key| key.contains(quiet)), "due releases, active production and never renewals never notify");
        }

        let severity = |key: &str| from_today.iter().find(|r| r.dedupe_key == key).unwrap().severity;
        assert_eq!(severity("today:review:release_review:00000000-0000-0000-0000-000000000001"), Severity::Info);
        assert_eq!(severity("today:failed:publication:00000000-0000-0000-0000-000000000002:outcome_unknown"), Severity::Warning);
        assert_eq!(severity("today:failed:generation:00000000-0000-0000-0000-000000000003:failed"), Severity::Critical);
        assert_eq!(severity("today:overdue:00000000-0000-0000-0000-000000000004"), Severity::Warning);
        assert_eq!(severity("today:renewal:00000000-0000-0000-0000-000000000006:overdue"), Severity::Critical);
        assert_eq!(severity("today:renewal:00000000-0000-0000-0000-000000000007:due"), Severity::Warning);
        assert_eq!(severity("today:renewal:00000000-0000-0000-0000-00000000000a:unknown"), Severity::Info);
        assert_eq!(severity("today:renewal:00000000-0000-0000-0000-00000000000b:unverifiable"), Severity::Warning);
        assert_eq!(severity("today:renewal:00000000-0000-0000-0000-00000000000c:invalid"), Severity::Critical);
        assert_eq!(severity("today:opportunity:00000000-0000-0000-0000-000000000005:shortlisted"), Severity::Info);
        assert!(from_fleet.iter().all(|r| r.severity == Severity::Warning && r.source == "fleet"));

        // Aged-out or unreadable snapshots are the same `fleet:stale` raise.
        let stale = fleet_raises(&status, generated + chrono::Duration::minutes(16));
        assert_eq!(stale.len(), from_fleet.len() + 1);
        assert!(stale.iter().any(|r| r.dedupe_key == "fleet:stale"));
        assert_eq!(fleet_unreadable("gone").dedupe_key, "fleet:stale");
    }

    #[test]
    fn producer_raises_are_stable_and_key_safe() {
        // The production nightly stamp carries fractional seconds and `+00:00`.
        let mut status: FleetStatus = serde_json::from_str(FLEET_SNAPSHOT).unwrap();
        status.nightly.as_mut().unwrap().started_at = "2026-11-15T02:00:00.766434605+00:00".into();
        let generated = DateTime::parse_from_rfc3339("2026-11-10T03:10:00Z").unwrap().with_timezone(&Utc);
        let pass = |minutes| fleet_raises(&status, generated + chrono::Duration::minutes(minutes));
        for raise in pass(20) {
            assert!(valid_key(&raise.dedupe_key), "{} breaks the key check", raise.dedupe_key);
        }
        assert!(pass(20).iter().any(|r| r.dedupe_key == "fleet:nightly:2026-11-15t02:00:00.766434605-00:00"));

        // A stale snapshot reads the same on every pass, so an acknowledgement
        // survives until a new snapshot arrives.
        let body = |minutes| pass(minutes).into_iter().find(|r| r.dedupe_key == "fleet:stale").unwrap().body;
        assert_eq!(body(20), body(90));

        // Two failed destinations of one content share a key: one raise, both named.
        let id = Uuid::from_u128(2);
        let mut a = today_item("publication", "content", id, "failed");
        a.title = "Publication to account b failed".into();
        let mut b = today_item("publication", "content", id, "failed");
        b.title = "Publication to account a failed".into();
        let today = |items: Vec<TodayItem>| Today { failed_operations: items, ..Today::default() };
        let merged = today_raises(&today(vec![a.clone(), b.clone()]));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged, today_raises(&today(vec![b, a])), "order of Today rows does not change the raise");
        assert!(merged[0].body.contains("account a") && merged[0].body.contains("account b"));
    }
}
