//! Principals, status, production pause and accounts.

use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use contracts::{Account, Audience, Capabilities, PauseIn, Principal, Role, SyncFailure, SystemStatus};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, Hub, OPERATOR, audit, decode, hash};

impl Hub {
    pub fn mark_worker_started(&self) {
        *self.worker_started_at.write().unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());
    }

    pub fn record_worker_tick(&self) {
        *self.worker_last_tick.write().unwrap_or_else(|e| e.into_inner()) = Some(Utc::now());
    }

    pub fn worker_health(&self, now: DateTime<Utc>, max_stall: Duration) -> Result<(), String> {
        if !self.worker_enabled.load(Ordering::Acquire) {
            return Ok(());
        }
        let started = *self.worker_started_at.read().unwrap_or_else(|e| e.into_inner());
        let tick = *self.worker_last_tick.read().unwrap_or_else(|e| e.into_inner());
        let latest = started.max(tick).ok_or_else(|| "worker has not started".to_string())?;
        if now.signed_duration_since(latest).to_std().is_ok_and(|age| age > max_stall) {
            return Err(format!("worker stalled; last activity at {}", latest.to_rfc3339()));
        }
        Ok(())
    }

    pub fn record_sync_ok(&self, source: &str) {
        let mut sources = self.sync_health.write().unwrap_or_else(|e| e.into_inner());
        let health = sources.entry(source.to_owned()).or_default();
        health.consecutive_failures = 0;
        health.last_ok_at = Some(Utc::now());
    }

    pub fn record_sync_error(&self, source: &str, err: impl ToString) {
        let err = err.to_string();
        let mut sources = self.sync_health.write().unwrap_or_else(|e| e.into_inner());
        let health = sources.entry(source.to_owned()).or_default();
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        health.last_error = Some(err);
        health.last_error_at = Some(Utc::now());
    }

    pub fn sync_failures(&self) -> Vec<SyncFailure> {
        self.sync_health
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, health)| health.consecutive_failures > 0)
            .map(|(source, health)| SyncFailure {
                source: source.clone(),
                consecutive_failures: health.consecutive_failures,
                last_error: health.last_error.clone(),
                last_ok_at: health.last_ok_at,
                last_error_at: health.last_error_at,
            })
            .collect()
    }
}

/// Resolve a bearer token to its principal. Only token hashes are stored.
pub async fn authenticate(hub: &Hub, token: &str) -> DResult<Option<Principal>> {
    let row: Option<(Uuid, String, String)> = sqlx::query_as("select id, name, role from principals where token_sha256 = $1")
        .bind(hash::sha256_hex(token.as_bytes()))
        .fetch_optional(&hub.db)
        .await?;
    Ok(row.and_then(|(id, name, role)| Role::parse(&role).map(|role| Principal { id, name, role })))
}

/// Create or rotate a principal's token: one live token per principal name,
/// so re-issuing (or a changed credential file) revokes the previous one while
/// preserving the principal's stable id.
pub async fn issue_token(db: &sqlx::PgPool, name: &str, role: Role, token: Option<String>) -> DResult<String> {
    let token = token.unwrap_or_else(|| format!("ihub_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
    let digest = hash::sha256_hex(token.as_bytes());
    let mut tx = db.begin().await?;
    sqlx::query("delete from principals where token_sha256 = $1 and name <> $2").bind(&digest).bind(name).execute(&mut *tx).await?;
    let updated = sqlx::query("update principals set role = $2, token_sha256 = $3 where name = $1")
        .bind(name)
        .bind(role.as_str())
        .bind(&digest)
        .execute(&mut *tx)
        .await?;
    if updated.rows_affected() == 0 {
        sqlx::query("insert into principals (id, name, role, token_sha256) values ($1, $2, $3, $4)")
            .bind(Uuid::new_v4())
            .bind(name)
            .bind(role.as_str())
            .bind(&digest)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(token)
}

pub async fn status(hub: &Hub, actor: &Actor) -> DResult<SystemStatus> {
    actor.require(&[Role::Operator, Role::Agent, Role::Procedure, Role::Ingest, Role::Member])?;
    let member = actor.principal.role == Role::Member;
    let operator = actor.principal.role == Role::Operator;
    let grant = hub.config.intel.grant(actor).ok();
    let owner = hub.resolve_owner(actor).is_ok();
    let paused = if member {
        false
    } else {
        let mut conn = hub.db.acquire().await?;
        crate::release::production_paused(&mut conn).await?
    };
    let intel_clear_audiences =
        if operator { Audience::ALL.to_vec() } else { grant.map(|g| g.clear_audiences.clone()).unwrap_or_default() };
    Ok(SystemStatus {
        api_version: contracts::API_VERSION.into(),
        fixture_mode: !member && hub.config.integrations.iter().any(|i| i.mode == contracts::AdapterMode::Fixture),
        production_paused: paused,
        principal: actor.principal.clone(),
        integrations: if member { Vec::new() } else { hub.config.integrations.clone() },
        feeds: if member { Vec::new() } else { hub.feeds.read().map(|f| f.clone()).unwrap_or_default() },
        capabilities: Some(Capabilities {
            intel_read: operator || grant.is_some(),
            intel_write: operator || grant.is_some_and(|g| !g.write_sources.is_empty()),
            intel_review: operator || grant.is_some_and(|g| g.can_review),
            intel_clear: !intel_clear_audiences.is_empty(),
            intel_clear_audiences,
            opportunities_manage: operator || crate::opportunities::steward_grant(hub, actor).is_some(),
            mail: owner,
            calendar: owner,
        }),
        sync_failures: if member { Vec::new() } else { hub.sync_failures() },
    })
}

pub async fn set_pause(hub: &Hub, actor: &Actor, input: PauseIn) -> DResult<SystemStatus> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    sqlx::query("update settings set value = $1, updated_at = now() where key = 'production_pause'")
        .bind(json!({ "paused": input.paused, "reason": input.reason, "by": actor.name() }))
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        actor,
        if input.paused { "system.paused" } else { "system.resumed" },
        "system",
        "production",
        json!({ "reason": input.reason }),
    )
    .await?;
    tx.commit().await?;
    status(hub, actor).await
}

pub async fn upsert_account(hub: &Hub, a: &Account) -> DResult<()> {
    sqlx::query(
        "insert into accounts (id, company_id, platform, handle, transport, persona_id, capabilities) values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (id) do update set platform = excluded.platform, handle = excluded.handle, transport = excluded.transport,
           persona_id = excluded.persona_id, capabilities = excluded.capabilities",
    )
    .bind(&a.id)
    .bind(&a.company_id)
    .bind(&a.platform)
    .bind(&a.handle)
    .bind(&a.transport)
    .bind(a.persona_id)
    .bind(serde_json::to_value(&a.capabilities).unwrap_or_default())
    .execute(&hub.db)
    .await?;
    Ok(())
}

pub async fn accounts(hub: &Hub, actor: &Actor) -> DResult<Vec<Account>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(t) from (select id, company_id, platform, handle, transport, persona_id, capabilities from accounts order by id) t",
    )
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

#[tokio::test]
async fn worker_heartbeat_and_sync_failures_are_shared_across_hub_clones() {
    let db = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://localhost/test").unwrap();
    let hub = crate::mail_calendar_tests::test_hub(db, Default::default());
    let worker = hub.clone();
    let window = Duration::from_secs(30);
    assert!(hub.worker_health(Utc::now(), window).is_ok());
    hub.worker_enabled.store(true, Ordering::Release);
    assert!(hub.worker_health(Utc::now(), window).is_err());
    worker.mark_worker_started();
    assert!(hub.worker_health(Utc::now(), window).is_ok());
    let later = Utc::now() + chrono::Duration::seconds(31);
    assert!(hub.worker_health(later, window).is_err());
    worker.record_worker_tick();
    assert!(hub.worker_health(Utc::now(), window).is_ok());

    hub.record_sync_ok("fleet");
    worker.record_sync_error("fleet", "first");
    worker.record_sync_error("fleet", "second");
    let failures = hub.sync_failures();
    assert_eq!(failures[0].consecutive_failures, 2);
    assert_eq!(failures[0].last_error.as_deref(), Some("second"));
    assert!(failures[0].last_ok_at <= failures[0].last_error_at);
    hub.record_sync_ok("fleet");
    assert!(worker.sync_failures().is_empty());
}
