use contracts::Severity;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{Raise, lock_writers};
use crate::{Actor, DResult, FleetIssues, Hub, hash, outbox};

pub(super) fn condition(key: &str) -> &str {
    if key.starts_with("fleet:nightly:") && key != "fleet:nightly:stalled" { "fleet:nightly" } else { key }
}

fn priority(key: &str, severity: &str) -> &'static str {
    if key.starts_with("fleet:nightly") || key.starts_with("fleet:offline:") || key.starts_with("fleet:probe:") {
        "medium"
    } else if key.starts_with("fleet:host:") && severity == "critical" {
        "critical"
    } else {
        "high"
    }
}

fn body_hash(raise: &Raise) -> String {
    hash::sha256_hex(json!([raise.severity, raise.title, raise.body]).to_string().as_bytes())
}

fn create_prefix(id: Uuid) -> String {
    format!("fleetix-hub:fleet:ep:{id}:create:")
}

#[derive(sqlx::FromRow)]
struct Episode {
    id: Uuid,
    notification_id: Uuid,
    paperclip_issue_id: Option<String>,
    paperclip_issue_url: Option<String>,
    last_synced_body_hash: String,
    last_synced_body: String,
    last_synced_severity: String,
}

pub(super) async fn reconcile_fleet_episode(tx: &mut PgConnection, company: &str, route: &FleetIssues, raise: &Raise) -> DResult<usize> {
    let key = raise.dedupe_key.as_str();
    if raise.severity == Severity::Info || key.starts_with("fleet:op:") || key.starts_with("fleet:warn:") {
        return Ok(0);
    }
    let condition = condition(key);
    let severity = raise.severity.as_str();
    let hash = body_hash(raise);
    let notification_id: Uuid = sqlx::query_scalar("select id from notifications where company_id = $1 and dedupe_key = $2")
        .bind(company)
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
    let episode: Option<Episode> = sqlx::query_as(
        "select id, notification_id, paperclip_issue_id, paperclip_issue_url,
                last_synced_body_hash, last_synced_body, last_synced_severity
         from fleet_issue_episodes where company_id = $1 and condition_key = $2 and state = 'open'",
    )
    .bind(company)
    .bind(condition)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(episode) = episode else {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into fleet_issue_episodes
             (id, company_id, dedupe_key, condition_key, notification_id, last_synced_body_hash, last_synced_body, last_synced_severity)
             values ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(company)
        .bind(key)
        .bind(condition)
        .bind(notification_id)
        .bind(&hash)
        .bind(&raise.body)
        .bind(severity)
        .execute(&mut *tx)
        .await?;
        enqueue_create(tx, id, notification_id, route, raise, &hash, 0).await?;
        return Ok(0);
    };

    let moved = notification_id != episode.notification_id;
    let changed = hash != episode.last_synced_body_hash;
    if moved || changed {
        // A new nightly run can replace the notification without being a recovery.
        sqlx::query(
            "update fleet_issue_episodes set notification_id = $2, dedupe_key = $3, last_synced_body_hash = $4,
                last_synced_body = $5, last_synced_severity = $6, updated_at = now() where id = $1",
        )
        .bind(episode.id)
        .bind(notification_id)
        .bind(key)
        .bind(&hash)
        .bind(&raise.body)
        .bind(severity)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(issue_id) = episode.paperclip_issue_id {
        if changed {
            enqueue_changes(
                tx,
                episode.id,
                &issue_id,
                &hash,
                key,
                (&episode.last_synced_body, &episode.last_synced_severity),
                (&raise.title, &raise.body, severity),
            )
            .await?;
        }
        if moved {
            return link_notification(tx, notification_id, episode.paperclip_issue_url.as_deref()).await;
        }
    } else {
        // Read only: the worker may hold a create row lock while acquiring our writer lock.
        let (active, dead, permanent): (bool, i64, bool) = sqlx::query_as(
            "select exists(select 1 from outbox where idempotency_key like $1 and state <> 'dead'),
               (select count(*) from outbox where idempotency_key like $1 and state = 'dead'),
               coalesce((select starts_with(last_error, $2) from outbox where idempotency_key like $1 and state = 'dead'
                         order by created_at desc limit 1), false)",
        )
        .bind(format!("{}%", create_prefix(episode.id)))
        .bind(outbox::PERMANENT_4XX_PREFIX)
        .fetch_one(&mut *tx)
        .await?;
        if !active && !permanent {
            enqueue_create(tx, episode.id, notification_id, route, raise, &hash, dead).await?;
        }
    }
    Ok(0)
}

async fn enqueue_create(
    tx: &mut PgConnection,
    episode_id: Uuid,
    notification_id: Uuid,
    route: &FleetIssues,
    raise: &Raise,
    hash: &str,
    generation: i64,
) -> DResult<()> {
    let condition = condition(&raise.dedupe_key);
    // Paperclip also folds same-titled creates into recently opened issues.
    // A recurrence must be a new issue even if its predecessor's close is delayed.
    let recurring: bool = sqlx::query_scalar(
        "select exists(select 1 from fleet_issue_episodes previous
          join fleet_issue_episodes current on current.id = $1
          where previous.company_id = current.company_id and previous.condition_key = current.condition_key and previous.state = 'resolved')",
    ).bind(episode_id).fetch_one(&mut *tx).await?;
    let title = if recurring { format!("Fleet: {condition} ({episode_id})") } else { format!("Fleet: {condition}") };
    let description = format!(
        "{}\n\n{}\n\nDetected by Fleetix Hub (notification `{}`). Diagnose from evidence, state the earliest causal failure and land the fix; the Board activates it.",
        raise.title, raise.body, raise.dedupe_key
    );
    outbox::enqueue(
        tx,
        &Actor::system(),
        outbox::PAPERCLIP_CREATE_ISSUE,
        &format!("{}g{generation}", create_prefix(episode_id)),
        json!({
            "episode_id": episode_id, "notification_id": notification_id, "company_id": route.company_id,
            "title": title, "description": description,
            "body": raise.body, "body_hash": hash, "severity": raise.severity,
            "priority": priority(&raise.dedupe_key, raise.severity.as_str()),
            "origin_key": format!("fleetix-hub:fleet:{condition}:{episode_id}"),
            "project_id": route.project_id, "assignee_agent_id": route.assignee_agent_id,
        }),
    )
    .await?;
    Ok(())
}

/// Episode id, notification id, dedupe key, title, synced body, severity, body hash, generations.
type DeadFleetCreate = (Uuid, Uuid, String, String, String, String, String, i64);

/// A healthy routing probe can revive rejected creates. Without a recovery
/// transition, only retry rows that were rejected under a different route.
pub async fn rearm_permanent_dead_fleet_creates(hub: &Hub, force_all: bool) -> DResult<usize> {
    let Some(route) = &hub.config.fleet_issues else { return Ok(0) };
    let mut tx = hub.db.begin().await?;
    lock_writers(&mut tx).await?;
    // Keys encode the generation; sort by length first to order g10 after g9.
    let dead: Vec<DeadFleetCreate> = sqlx::query_as(
        "select e.id, e.notification_id, e.dedupe_key, n.title, e.last_synced_body,
                e.last_synced_severity, e.last_synced_body_hash, latest.generations
           from fleet_issue_episodes e join notifications n on n.id = e.notification_id
           cross join lateral (
               select state, payload, last_error, count(*) over () as generations
                 from outbox where idempotency_key like 'fleetix-hub:fleet:ep:' || e.id || ':create:g%'
                 order by length(idempotency_key) desc, idempotency_key desc limit 1
           ) latest
          where e.company_id = $1 and e.state = 'open' and e.paperclip_issue_id is null
            and latest.state = 'dead' and starts_with(latest.last_error, $2)
            and ($3 or (latest.payload->>'company_id', latest.payload->>'project_id', latest.payload->>'assignee_agent_id')
                       is distinct from ($4::text, $5::text, $6::text))",
    )
    .bind(&hub.config.company_id)
    .bind(outbox::PERMANENT_4XX_PREFIX)
    .bind(force_all)
    .bind(&route.company_id)
    .bind(&route.project_id)
    .bind(&route.assignee_agent_id)
    .fetch_all(&mut *tx)
    .await?;
    let count = dead.len();
    for (episode_id, notification_id, dedupe_key, title, body, severity, hash, generation) in dead {
        let raise = Raise {
            source: "fleet".into(),
            dedupe_key,
            severity: Severity::parse(&severity).expect("fleet episode severity is constrained"),
            title,
            body,
            subject_type: None,
            subject_id: None,
            url: None,
        };
        enqueue_create(&mut tx, episode_id, notification_id, route, &raise, &hash, generation).await?;
    }
    tx.commit().await?;
    Ok(count)
}

fn delta(previous: &str, title: &str, body: &str, severity: &str) -> String {
    use std::collections::BTreeSet;
    let old: BTreeSet<_> = previous.lines().filter(|line| !line.is_empty()).collect();
    let new: BTreeSet<_> = body.lines().filter(|line| !line.is_empty()).collect();
    let mut comment = String::new();
    for (label, lines) in [("Added", new.difference(&old)), ("Removed", old.difference(&new))] {
        let mut lines = lines.peekable();
        if lines.peek().is_some() {
            comment.push_str(&format!("**{label}:**\n"));
            for line in lines {
                comment.push_str(&format!("- {line}\n"));
            }
            comment.push('\n');
        }
    }
    comment.push_str(&format!("**Current state ({severity}):** {title}\n\n{body}"));
    comment
}

async fn enqueue_changes(
    tx: &mut PgConnection,
    episode_id: Uuid,
    issue_id: &str,
    hash: &str,
    key: &str,
    (previous_body, previous_severity): (&str, &str),
    (title, body, severity): (&str, &str, &str),
) -> DResult<()> {
    let (old_priority, new_priority) = (priority(key, previous_severity), priority(key, severity));
    let comment = delta(previous_body, title, body, severity);
    let mut key_suffix = hash[..16].to_string();
    let prefix = format!("fleetix-hub:fleet:ep:{episode_id}");
    let seen: bool = sqlx::query_scalar("select exists(select 1 from outbox where idempotency_key = $1)")
        .bind(format!("{prefix}:upd:{key_suffix}"))
        .fetch_one(&mut *tx)
        .await?;
    if seen {
        // A → B → A → B is three changes, not a retry of the first B.
        key_suffix.push_str(&format!(":{}", Uuid::new_v4()));
    }
    outbox::enqueue(
        tx,
        &Actor::system(),
        outbox::PAPERCLIP_COMMENT_ISSUE,
        &format!("{prefix}:upd:{key_suffix}"),
        json!({"episode_id": episode_id, "issue_id": issue_id, "body": comment}),
    )
    .await?;
    if old_priority != new_priority {
        outbox::enqueue(
            tx,
            &Actor::system(),
            outbox::PAPERCLIP_UPDATE_ISSUE,
            &format!("{prefix}:prio:{key_suffix}"),
            json!({"episode_id": episode_id, "issue_id": issue_id, "priority": new_priority}),
        )
        .await?;
    }
    Ok(())
}

async fn link_notification(tx: &mut PgConnection, id: Uuid, url: Option<&str>) -> DResult<usize> {
    let changed = sqlx::query(
        "update notifications set url = $2, seq = nextval('notifications_seq'), updated_at = now()
         where id = $1 and url is distinct from $2",
    )
    .bind(id)
    .bind(url)
    .execute(tx)
    .await?
    .rows_affected();
    Ok(changed as usize)
}

pub(super) async fn resolve(tx: &mut PgConnection, notification_id: Uuid, key: &str, title: &str) -> DResult<()> {
    let closed: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "update fleet_issue_episodes set state = 'resolved', resolved_at = now(), updated_at = now()
         where notification_id = $1 and state = 'open' returning id, paperclip_issue_id",
    )
    .bind(notification_id)
    .fetch_all(&mut *tx)
    .await?;
    for (episode_id, issue_id) in closed {
        if let Some(issue_id) = issue_id {
            enqueue_recovery(tx, episode_id, &issue_id, key, title).await?;
        }
    }
    Ok(())
}

async fn enqueue_recovery(tx: &mut PgConnection, episode_id: Uuid, issue_id: &str, key: &str, title: &str) -> DResult<()> {
    outbox::enqueue(
        tx, &Actor::system(), outbox::PAPERCLIP_UPDATE_ISSUE, &format!("fleetix-hub:fleet:ep:{episode_id}:resolve"),
        json!({"episode_id": episode_id, "issue_id": issue_id, "status": "done", "comment": format!("Fleet condition cleared: {title} (`{key}`).")}),
    ).await?;
    Ok(())
}

/// Replay the same close evidence after an outage exhausts its retry budget.
pub(super) async fn rearm_dead_recoveries(tx: &mut PgConnection, company_id: &str) -> DResult<()> {
    let dead: Vec<(Uuid, Value, i64)> = sqlx::query_as(
        "select e.id, latest.payload, latest.generations
           from fleet_issue_episodes e
           cross join lateral (
               select last_error, payload, count(*) over () as generations, bool_and(state = 'dead') over () as all_dead
                 from outbox
                where idempotency_key = 'fleetix-hub:fleet:ep:' || e.id || ':resolve'
                   or idempotency_key like 'fleetix-hub:fleet:ep:' || e.id || ':resolve:g%'
                order by length(idempotency_key) desc, idempotency_key desc limit 1
           ) latest
          where e.company_id = $1 and e.state = 'resolved' and e.paperclip_issue_id is not null
            and latest.all_dead and not coalesce(starts_with(latest.last_error, $2), false)",
    )
    .bind(company_id)
    .bind(outbox::PERMANENT_4XX_PREFIX)
    .fetch_all(&mut *tx)
    .await?;
    for (episode_id, payload, generation) in dead {
        outbox::enqueue(
            tx,
            &Actor::system(),
            outbox::PAPERCLIP_UPDATE_ISSUE,
            &format!("fleetix-hub:fleet:ep:{episode_id}:resolve:g{generation}"),
            payload,
        )
        .await?;
    }
    Ok(())
}

/// Link a create receipt under the notification writer lock. The queued
/// notification id can be older than the active nightly run's notification.
pub async fn record_fleet_episode_issue(
    pool: &PgPool,
    episode_id: Uuid,
    notification_id: Uuid,
    issue_id: &str,
    issue_url: Option<&str>,
) -> DResult<()> {
    let mut tx = pool.begin().await?;
    lock_writers(&mut tx).await?;
    let receipt: Option<(Uuid, String, String, String, String)> = sqlx::query_as(
        "update fleet_issue_episodes set paperclip_issue_id = $2, paperclip_issue_url = $3, updated_at = now()
         where id = $1 and paperclip_issue_id is null
         returning notification_id, state, last_synced_body_hash, last_synced_body, last_synced_severity",
    )
    .bind(episode_id)
    .bind(issue_id)
    .bind(issue_url)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((active_id, state, hash, body, severity)) = receipt {
        let (key, title): (String, String) =
            sqlx::query_as("select dedupe_key, title from notifications where id = $1").bind(active_id).fetch_one(&mut *tx).await?;
        if state == "resolved" {
            // The remote create won the race with recovery. Close this issue,
            // never overwrite the link of a new episode of the same condition.
            enqueue_recovery(&mut tx, episode_id, issue_id, &key, &title).await?;
        } else {
            link_notification(&mut tx, active_id, issue_url).await?;
            let sent: Option<(String, String, String)> = sqlx::query_as(
                "select payload->>'body_hash', payload->>'body', payload->>'severity' from outbox
                 where idempotency_key like $1 and state <> 'dead' and payload->>'notification_id' = $2",
            )
            .bind(format!("{}%", create_prefix(episode_id)))
            .bind(notification_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some((sent_hash, old_body, old_severity)) = sent
                && sent_hash != hash
            {
                enqueue_changes(&mut tx, episode_id, issue_id, &hash, &key, (&old_body, &old_severity), (&title, &body, &severity)).await?;
            }
        }
    } else {
        let linked: bool =
            sqlx::query_scalar("select exists(select 1 from fleet_issue_episodes where id = $1 and paperclip_issue_id = $2)")
                .bind(episode_id)
                .bind(issue_id)
                .fetch_one(&mut *tx)
                .await?;
        if !linked {
            tracing::warn!(%episode_id, %issue_id, "Paperclip create receipt has no matching fleet episode");
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HubConfig;
    use crate::mail_calendar_tests::{test_database, test_hub};
    use crate::notifications::{clear_paperclip_routing_error, report_paperclip_routing_error, sync, sync_keeping};
    use serde_json::Value;

    fn raised(key: &str, severity: Severity, body: &str) -> Raise {
        Raise {
            source: "fleet".into(),
            dedupe_key: format!("fleet:{key}"),
            severity,
            title: key.into(),
            body: body.into(),
            subject_type: None,
            subject_id: None,
            url: None,
        }
    }

    #[test]
    fn priorities_and_condition_keys_distinguish_fleet_signals() {
        for (key, severity, expected) in [
            ("host:nomad", "critical", "critical"),
            ("host:nomad", "warning", "high"),
            ("stale", "critical", "high"),
            ("monitoring", "critical", "high"),
            ("alert:a", "warning", "high"),
            ("nightly:2026-11-25", "warning", "medium"),
            ("nightly:stalled", "warning", "medium"),
            ("offline:nomad", "warning", "medium"),
            ("probe:nomad", "warning", "medium"),
        ] {
            assert_eq!(priority(&format!("fleet:{key}"), severity), expected, "{key}");
        }
        assert_eq!(condition("fleet:nightly:stalled"), "fleet:nightly:stalled");
        assert_eq!(condition("fleet:nightly:2026-11-25"), "fleet:nightly");
        let comment = delta("disk\nunit", "Host is degraded", "gpu\nunit", "warning");
        assert!(comment.contains("**Added:**\n- gpu\n\n**Removed:**\n- disk"));
        assert!(comment.ends_with("**Current state (warning):** Host is degraded\n\ngpu\nunit"));
    }

    async fn payloads(db: &PgPool, episode: Uuid, topic: &str) -> Vec<Value> {
        sqlx::query_scalar("select payload from outbox where topic = $1 and payload->>'episode_id' = $2 order by created_at, id")
            .bind(topic)
            .bind(episode.to_string())
            .fetch_all(db)
            .await
            .unwrap()
    }

    async fn open_episode(db: &PgPool, condition: &str) -> (Uuid, Uuid) {
        sqlx::query_as("select id, notification_id from fleet_issue_episodes where condition_key = $1 and state = 'open'")
            .bind(condition)
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn notification(db: &PgPool, key: &str) -> (bool, Option<String>) {
        sqlx::query_as("select resolved_at is not null, url from notifications where dedupe_key = $1")
            .bind(key)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fleet_episode_postgres_lifecycle_and_dead_letter_policy() {
        let Some((db, admin, database)) = test_database().await else {
            eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
            return;
        };
        let hub = test_hub(
            db.clone(),
            HubConfig {
                company_id: "hub".into(),
                fleet_issues: Some(FleetIssues { company_id: "ops".into(), project_id: Some("project".into()), assignee_agent_id: None }),
                ..Default::default()
            },
        );
        let old = raised("host:nomad", Severity::Warning, "disk");
        let new = raised("host:nomad", Severity::Critical, "gpu");
        let mut duplicate = old.clone();
        duplicate.dedupe_key = "FLEET:HOST:NOMAD".into();
        let first = vec![
            old.clone(),
            duplicate,
            raised("host:nomad-dev", Severity::Warning, "disk"),
            raised("warn:nomad", Severity::Info, "warning"),
            raised("op:abc", Severity::Warning, "pending"),
        ];
        assert_eq!(sync(&hub, "fleet", first.clone()).await.unwrap(), 4);
        assert_eq!(sync(&hub, "fleet", first).await.unwrap(), 0);
        let count: i64 = sqlx::query_scalar("select count(*) from fleet_issue_episodes").fetch_one(&db).await.unwrap();
        assert_eq!(count, 2, "warnings and approval waits do not file");
        let (ep, note) = open_episode(&db, "fleet:host:nomad").await;
        for invalid in ["last_synced_severity = 'unknown'", "state = 'resolved'", "resolved_at = now()"] {
            let error =
                sqlx::query(&format!("update fleet_issue_episodes set {invalid} where id = $1")).bind(ep).execute(&db).await.unwrap_err();
            assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23514"));
        }
        let creates = payloads(&db, ep, outbox::PAPERCLIP_CREATE_ISSUE).await;
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0]["priority"], "high");
        sync_keeping(&hub, "fleet", vec![], &["fleet:host:nomad".into(), "fleet:warn:".into()]).await.unwrap();
        assert!(!notification(&db, "fleet:host:nomad").await.0);
        assert!(notification(&db, "fleet:host:nomad-dev").await.0);
        assert!(!notification(&db, "fleet:warn:nomad").await.0);

        // Changes while the create is in flight are not lost at receipt time.
        sync(&hub, "fleet", vec![new.clone()]).await.unwrap();
        record_fleet_episode_issue(&db, ep, note, "ops-1", Some("https://board/issues/OPS-1")).await.unwrap();
        assert_eq!(notification(&db, "fleet:host:nomad").await, (false, Some("https://board/issues/OPS-1".into())));
        let comments = payloads(&db, ep, outbox::PAPERCLIP_COMMENT_ISSUE).await;
        assert_eq!(comments.len(), 1);
        let comment = comments[0]["body"].as_str().unwrap();
        assert!(comment.contains("- gpu") && comment.contains("- disk"));
        assert_eq!(payloads(&db, ep, outbox::PAPERCLIP_UPDATE_ISSUE).await[0]["priority"], "critical");
        for raise in [new.clone(), old.clone(), new.clone(), new.clone()] {
            sync(&hub, "fleet", vec![raise]).await.unwrap();
        }
        assert_eq!(payloads(&db, ep, outbox::PAPERCLIP_COMMENT_ISSUE).await.len(), 3, "only actual transitions enqueue");
        assert!(notification(&db, "fleet:host:nomad").await.1.is_some(), "fault updates preserve the issue URL");
        sync(&hub, "fleet", vec![]).await.unwrap();
        let updates = payloads(&db, ep, outbox::PAPERCLIP_UPDATE_ISSUE).await;
        assert!(updates.iter().any(|p| p["status"] == "done" && p["comment"].as_str().unwrap().contains("Fleet condition cleared")));
        sync(&hub, "fleet", vec![old.clone()]).await.unwrap();
        let (next, next_note) = open_episode(&db, "fleet:host:nomad").await;
        assert_ne!(next, ep);
        assert_ne!(payloads(&db, next, outbox::PAPERCLIP_CREATE_ISSUE).await[0]["title"], creates[0]["title"]);
        assert_eq!(next_note, note);
        assert_eq!(notification(&db, "fleet:host:nomad").await, (false, None));

        // Only exhausted transient creates are re-armed, with new payloads but
        // the same remote idempotency key in case a previous attempt succeeded.
        let id_for = |generation| format!("{}g{generation}", create_prefix(next));
        let row: Uuid = sqlx::query_scalar("update outbox set attempts = $2 where idempotency_key = $1 returning id")
            .bind(id_for(0))
            .bind(outbox::MAX_ATTEMPTS - 2)
            .fetch_one(&db)
            .await
            .unwrap();
        let mut conn = db.acquire().await.unwrap();
        assert!(!outbox::fail(&mut conn, row, "network", false).await.unwrap());
        assert!(outbox::fail(&mut conn, row, "network", false).await.unwrap());
        assert!(!outbox::fail(&mut conn, row, "network", false).await.unwrap());
        sync(&hub, "fleet", vec![new.clone()]).await.unwrap();
        let (id, retry): (Uuid, Value) =
            sqlx::query_as("select id, payload from outbox where idempotency_key = $1").bind(id_for(1)).fetch_one(&db).await.unwrap();
        assert_eq!(retry["priority"], "critical");
        assert!(retry["description"].as_str().unwrap().contains("gpu"));
        assert_eq!(retry["origin_key"], payloads(&db, next, outbox::PAPERCLIP_CREATE_ISSUE).await[0]["origin_key"]);
        assert!(outbox::fail(&mut conn, id, "permanent 4xx: 404 invalid route", true).await.unwrap());
        sync(&hub, "fleet", vec![new]).await.unwrap();
        assert_eq!(payloads(&db, next, outbox::PAPERCLIP_CREATE_ISSUE).await.len(), 2);
        drop(conn);
        let mut restarted = hub.clone();
        let route = std::sync::Arc::make_mut(&mut restarted.config).fleet_issues.as_mut().unwrap();
        route.company_id = "new-company".into();
        route.project_id = None;
        route.assignee_agent_id = Some("new-agent".into());
        assert_eq!(rearm_permanent_dead_fleet_creates(&restarted, false).await.unwrap(), 1);
        assert_eq!(rearm_permanent_dead_fleet_creates(&restarted, true).await.unwrap(), 0, "do not replace an active create");
        let creates = payloads(&db, next, outbox::PAPERCLIP_CREATE_ISSUE).await;
        let latest = creates.last().unwrap();
        assert_eq!(latest["company_id"], "new-company");
        assert!(latest["project_id"].is_null());
        assert_eq!(latest["assignee_agent_id"], "new-agent");
        assert_eq!(latest["origin_key"], retry["origin_key"]);

        report_paperclip_routing_error(&hub, "invalid route").await.unwrap();
        assert!(!notification(&db, "paperclip:routing:error").await.0);
        clear_paperclip_routing_error(&hub).await.unwrap();
        assert!(notification(&db, "paperclip:routing:error").await.0);

        // A slow receipt from a resolved episode closes its own issue but cannot
        // steal the current notification's URL.
        sync(&hub, "fleet", vec![]).await.unwrap();
        sync(&hub, "fleet", vec![old]).await.unwrap();
        record_fleet_episode_issue(&db, next, note, "slow", Some("https://board/slow")).await.unwrap();
        assert_eq!(notification(&db, "fleet:host:nomad").await, (false, None));
        assert_eq!(payloads(&db, next, outbox::PAPERCLIP_UPDATE_ISSUE).await[0]["status"], "done");

        let nightly = |day| raised(&format!("nightly:2026-11-{day}"), Severity::Warning, "failed");
        sync(&hub, "fleet", vec![nightly(24)]).await.unwrap();
        let (night_ep, night_note) = open_episode(&db, "fleet:nightly").await;
        sync(&hub, "fleet", vec![nightly(25)]).await.unwrap();
        let (moved_ep, moved_note) = open_episode(&db, "fleet:nightly").await;
        assert_eq!(night_ep, moved_ep);
        assert_ne!(night_note, moved_note);
        assert!(notification(&db, "fleet:nightly:2026-11-24").await.0);
        record_fleet_episode_issue(&db, night_ep, night_note, "night", Some("https://board/night")).await.unwrap();
        assert_eq!(notification(&db, "fleet:nightly:2026-11-25").await, (false, Some("https://board/night".into())));
        assert_eq!(payloads(&db, night_ep, outbox::PAPERCLIP_CREATE_ISSUE).await.len(), 1);
        assert!(payloads(&db, night_ep, outbox::PAPERCLIP_UPDATE_ISSUE).await.is_empty());

        assert_eq!(crate::migration_status(&db).await.unwrap(), crate::MigrationStatus::default());
        sqlx::query(
            "insert into _sqlx_migrations (version, description, success, checksum, execution_time)
                     values (9999, 'ahead', true, '', 0), (10000, 'failed', false, '', 0)",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(crate::migration_status(&db).await.unwrap().ahead, vec![9999]);
        db.close().await;
        sqlx::query(&format!("drop database {database}")).execute(&admin).await.unwrap();
    }
}
