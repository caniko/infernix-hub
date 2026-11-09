//! Principals, status, production pause and accounts.

use contracts::{Account, PauseIn, Principal, Role, SystemStatus};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, Hub, OPERATOR, audit, decode, hash};

/// Resolve a bearer token to its principal. Only token hashes are stored.
pub async fn authenticate(hub: &Hub, token: &str) -> DResult<Option<Principal>> {
    let row: Option<(Uuid, String, String)> = sqlx::query_as("select id, name, role from principals where token_sha256 = $1")
        .bind(hash::sha256_hex(token.as_bytes()))
        .fetch_optional(&hub.db)
        .await?;
    Ok(row.and_then(|(id, name, role)| Role::parse(&role).map(|role| Principal { id, name, role })))
}

/// Create or rotate a principal's token: one live token per principal name,
/// so re-issuing (or a changed credential file) revokes the previous one.
pub async fn issue_token(db: &sqlx::PgPool, name: &str, role: Role, token: Option<String>) -> DResult<String> {
    let token = token.unwrap_or_else(|| format!("ihub_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()));
    let mut tx = db.begin().await?;
    sqlx::query("delete from principals where name = $1 or token_sha256 = $2")
        .bind(name)
        .bind(hash::sha256_hex(token.as_bytes()))
        .execute(&mut *tx)
        .await?;
    sqlx::query("insert into principals (id, name, role, token_sha256) values ($1, $2, $3, $4)")
        .bind(Uuid::new_v4())
        .bind(name)
        .bind(role.as_str())
        .bind(hash::sha256_hex(token.as_bytes()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(token)
}

pub async fn status(hub: &Hub, actor: &Actor) -> DResult<SystemStatus> {
    let mut conn = hub.db.acquire().await?;
    let paused = crate::release::production_paused(&mut conn).await?;
    Ok(SystemStatus {
        api_version: contracts::API_VERSION.into(),
        fixture_mode: hub.config.integrations.iter().any(|i| i.mode == contracts::AdapterMode::Fixture),
        production_paused: paused,
        principal: actor.principal.clone(),
        integrations: hub.config.integrations.clone(),
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
