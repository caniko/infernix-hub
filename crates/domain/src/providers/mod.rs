//! Generic AI model provider domain layer: multi-account inventory,
//! quota observations, weighted pool capacity, reset-aware forecasting,
//! past windows, capability-gated operations, and Spillway-parity analytics.

pub mod activity;
pub mod capture;
pub mod pricing;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use contracts::providers::*;
use contracts::{AdapterMode, Severity, Verification};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ports::{ProviderDispatch, ProviderIntent};
use crate::{ANY_READER, Actor, DResult, DomainError, Hub, OPERATOR, OPERATOR_OR_COLLECTOR, audit, conflict, hash, invalid};

/// Internal operator identity for in-process Hub seeding, post-operation refresh,
/// and background polling. External callers always authenticate through bearer tokens.
pub fn internal_operator(name: &str) -> Actor {
    Actor { principal: contracts::Principal { id: Uuid::nil(), name: name.into(), role: contracts::Role::Operator }, correlation_id: None }
}

/// Register all in-process `Hub.providers` descriptors into `model_provider_instances`
/// so they are immediately visible on `GET /v1/providers` even before their first
/// observation arrives.
pub async fn sync_registered_providers(hub: &Hub) -> DResult<()> {
    for provider in hub.providers.values() {
        let d = provider.descriptor();
        upsert_provider_descriptor(&hub.db, &d).await?;
    }
    Ok(())
}

async fn upsert_provider_descriptor(db: &sqlx::PgPool, d: &ProviderDescriptor) -> DResult<()> {
    let caps = serde_json::to_value(&d.capabilities).unwrap_or_else(|_| json!([]));
    let evs = serde_json::to_value(&d.evidence_kinds).unwrap_or_else(|_| json!([]));
    let health = serde_json::to_value(d.health).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "unobserved".into());
    sqlx::query(
        "insert into model_provider_instances
            (id, label, family, mode, verification, router_id, health, status_message, capabilities, evidence_kinds, last_observed_at, updated_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
         on conflict (id) do update set
            label = excluded.label,
            family = excluded.family,
            mode = excluded.mode,
            verification = excluded.verification,
            router_id = coalesce(excluded.router_id, model_provider_instances.router_id),
            health = case when excluded.last_observed_at is not null then excluded.health else model_provider_instances.health end,
            status_message = coalesce(excluded.status_message, model_provider_instances.status_message),
            capabilities = excluded.capabilities,
            evidence_kinds = excluded.evidence_kinds,
            last_observed_at = coalesce(excluded.last_observed_at, model_provider_instances.last_observed_at),
            updated_at = now()",
    )
    .bind(&d.id)
    .bind(&d.label)
    .bind(&d.family)
    .bind(d.mode.as_str())
    .bind(d.verification.as_str())
    .bind(&d.router_id)
    .bind(&health)
    .bind(&d.status_message)
    .bind(&caps)
    .bind(&evs)
    .bind(d.last_observed_at)
    .execute(db)
    .await?;
    Ok(())
}

/// Ingest a sanitized observation batch pushed by a user-session collector or pulled
/// by a server-side `ModelProvider`. Commits provider, accounts, quota samples,
/// reset grants, key groups, discovered models, and routing state atomically.
pub async fn ingest_observations(hub: &Hub, actor: &Actor, provider_id: &str, batch: ObservationBatch) -> DResult<ObservationAck> {
    actor.require(OPERATOR_OR_COLLECTOR)?;
    if batch.provider_id != provider_id {
        return Err(invalid("provider_id_mismatch", "path provider_id does not match batch.provider_id"));
    }
    enforce_collector_scope(hub, actor, &batch.source_host, &batch.source_user, provider_id).await?;

    let mut tx = hub.db.begin().await?;

    let (mode, verification) = hub
        .providers
        .get(provider_id)
        .map(|p| {
            let d = p.descriptor();
            (d.mode, d.verification)
        })
        .unwrap_or((AdapterMode::Live, Verification::LiveVerified));

    let health_str = serde_json::to_value(batch.health).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "healthy".into());
    let caps_json = serde_json::to_value(&batch.capabilities).unwrap_or_else(|_| json!([]));
    let evs_json = serde_json::to_value(&batch.evidence_kinds).unwrap_or_else(|_| json!([]));

    sqlx::query(
        "insert into model_provider_instances
            (id, label, family, mode, verification, router_id, health, status_message, capabilities, evidence_kinds, last_observed_at, updated_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
         on conflict (id) do update set
            label = excluded.label,
            family = excluded.family,
            router_id = coalesce(excluded.router_id, model_provider_instances.router_id),
            health = excluded.health,
            status_message = excluded.status_message,
            capabilities = case when jsonb_array_length(excluded.capabilities) > 0 then excluded.capabilities else model_provider_instances.capabilities end,
            evidence_kinds = case when jsonb_array_length(excluded.evidence_kinds) > 0 then excluded.evidence_kinds else model_provider_instances.evidence_kinds end,
            last_observed_at = excluded.last_observed_at,
            updated_at = now()",
    )
    .bind(provider_id)
    .bind(&batch.provider_label)
    .bind(&batch.provider_family)
    .bind(mode.as_str())
    .bind(verification.as_str())
    .bind(&batch.router_id)
    .bind(&health_str)
    .bind(&batch.status_message)
    .bind(&caps_json)
    .bind(&evs_json)
    .bind(batch.collected_at)
    .execute(&mut *tx)
    .await?;

    let mut accounts_upserted = 0usize;
    let mut samples_inserted = 0usize;
    let mut grants_upserted = 0usize;

    for acct in &batch.accounts {
        let masked = acct.masked_email.clone().or_else(|| acct.email.as_deref().map(mask_email));
        let enroll_str = serde_json::to_value(acct.enrollment_state)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unobserved".into());
        let acct_caps = serde_json::to_value(&acct.capabilities).unwrap_or_else(|_| json!([]));
        let starter_json = acct.rolling_starter.as_ref().and_then(|s| serde_json::to_value(s).ok());
        sqlx::query(
            "insert into model_accounts
                (id, provider_id, source_account_id, display_name, email, masked_email, note, plan,
                 tier_multiplier, enabled, priority, weight, cooldown_until, status_message,
                 is_active, is_preferred, is_next_pick, enrollment_state, host, unix_user,
                 credential_fingerprint, last_used_at, last_observed_at, capabilities,
                 success_count, failure_count, rolling_starter, updated_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8,
                     $9, $10, $11, $12, $13, $14,
                     $15, $16, $17, $18, $19, $20,
                     $21, $22, $23, $24,
                     $25, $26, $27, now())
             on conflict (provider_id, id) do update set
                source_account_id = excluded.source_account_id,
                display_name = excluded.display_name,
                email = coalesce(excluded.email, model_accounts.email),
                masked_email = coalesce(excluded.masked_email, model_accounts.masked_email),
                note = coalesce(excluded.note, model_accounts.note),
                plan = coalesce(excluded.plan, model_accounts.plan),
                tier_multiplier = coalesce(excluded.tier_multiplier, model_accounts.tier_multiplier),
                enabled = excluded.enabled,
                priority = excluded.priority,
                weight = excluded.weight,
                cooldown_until = excluded.cooldown_until,
                status_message = excluded.status_message,
                is_active = excluded.is_active,
                is_preferred = excluded.is_preferred,
                is_next_pick = excluded.is_next_pick,
                enrollment_state = excluded.enrollment_state,
                host = coalesce(excluded.host, model_accounts.host),
                unix_user = coalesce(excluded.unix_user, model_accounts.unix_user),
                credential_fingerprint = coalesce(excluded.credential_fingerprint, model_accounts.credential_fingerprint),
                last_used_at = coalesce(excluded.last_used_at, model_accounts.last_used_at),
                last_observed_at = coalesce(excluded.last_observed_at, excluded.updated_at),
                capabilities = excluded.capabilities,
                success_count = greatest(model_accounts.success_count, excluded.success_count),
                failure_count = greatest(model_accounts.failure_count, excluded.failure_count),
                rolling_starter = coalesce(excluded.rolling_starter, model_accounts.rolling_starter),
                updated_at = now()",
        )
        .bind(&acct.id)
        .bind(provider_id)
        .bind(&acct.source_account_id)
        .bind(&acct.display_name)
        .bind(&acct.email)
        .bind(&masked)
        .bind(&acct.note)
        .bind(&acct.plan)
        .bind(acct.tier_multiplier)
        .bind(acct.enabled)
        .bind(acct.priority)
        .bind(acct.weight as i32)
        .bind(acct.cooldown_until)
        .bind(&acct.status_message)
        .bind(acct.is_active)
        .bind(acct.is_preferred)
        .bind(acct.is_next_pick)
        .bind(&enroll_str)
        .bind(acct.host.as_deref().unwrap_or(&batch.source_host))
        .bind(acct.unix_user.as_deref().unwrap_or(&batch.source_user))
        .bind(&acct.credential_fingerprint)
        .bind(acct.last_used_at)
        .bind(acct.last_observed_at.unwrap_or(batch.collected_at))
        .bind(&acct_caps)
        .bind(acct.success_count as i64)
        .bind(acct.failure_count as i64)
        .bind(&starter_json)
        .execute(&mut *tx)
        .await?;
        accounts_upserted += 1;

        for win in &acct.windows {
            insert_quota_sample(&mut tx, provider_id, &acct.id, win, win.observed_at.unwrap_or(batch.collected_at)).await?;
            samples_inserted += 1;
        }
        if let Some(grant) = &acct.reset_grant {
            upsert_reset_grant(&mut tx, provider_id, &acct.id, grant).await?;
            grants_upserted += 1;
        }
    }

    for sample in &batch.quota_samples {
        insert_quota_sample(&mut tx, provider_id, &sample.account_id, &sample.window, sample.observed_at).await?;
        samples_inserted += 1;
    }

    for (acct_id, grant) in &batch.reset_grants {
        upsert_reset_grant(&mut tx, provider_id, acct_id, grant).await?;
        grants_upserted += 1;
    }

    for group in &batch.key_groups {
        let req_id =
            serde_json::to_value(group.request_identity).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "api".into());
        sqlx::query(
            "insert into model_key_groups
                (id, provider_id, family, name, base_url, enabled, request_identity, keys, configured_models, discovered_models, aliases, updated_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             on conflict (provider_id, id) do update set
                family = excluded.family,
                name = excluded.name,
                base_url = excluded.base_url,
                enabled = excluded.enabled,
                request_identity = excluded.request_identity,
                keys = excluded.keys,
                configured_models = excluded.configured_models,
                discovered_models = excluded.discovered_models,
                aliases = excluded.aliases,
                updated_at = excluded.updated_at",
        )
        .bind(&group.id)
        .bind(provider_id)
        .bind(&group.family)
        .bind(&group.name)
        .bind(&group.base_url)
        .bind(group.enabled)
        .bind(&req_id)
        .bind(serde_json::to_value(&group.keys).unwrap_or_else(|_| json!([])))
        .bind(serde_json::to_value(&group.configured_models).unwrap_or_else(|_| json!([])))
        .bind(serde_json::to_value(&group.discovered_models).unwrap_or_else(|_| json!([])))
        .bind(serde_json::to_value(&group.aliases).unwrap_or_else(|_| json!([])))
        .bind(group.updated_at)
        .execute(&mut *tx)
        .await?;
    }

    for model in &batch.models {
        sqlx::query(
            "insert into model_catalog
                (provider_id, model_id, display_name, family, group_id, context_window, supports_thinking, discovered_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8)
             on conflict (provider_id, model_id) do update set
                display_name = coalesce(excluded.display_name, model_catalog.display_name),
                family = coalesce(excluded.family, model_catalog.family),
                group_id = coalesce(excluded.group_id, model_catalog.group_id),
                context_window = coalesce(excluded.context_window, model_catalog.context_window),
                supports_thinking = coalesce(excluded.supports_thinking, model_catalog.supports_thinking),
                discovered_at = coalesce(excluded.discovered_at, model_catalog.discovered_at)",
        )
        .bind(provider_id)
        .bind(&model.id)
        .bind(&model.display_name)
        .bind(&model.family)
        .bind(&model.group_id)
        .bind(model.context_window.map(|v| v as i64))
        .bind(model.supports_thinking)
        .bind(model.discovered_at.or(Some(batch.collected_at)))
        .execute(&mut *tx)
        .await?;
    }

    if let Some(routing) = &batch.routing_status {
        let clean_logs: Vec<ProxyLogEntry> = routing
            .recent_logs
            .iter()
            .chain(batch.proxy_logs.iter())
            .take(500)
            .map(|entry| ProxyLogEntry {
                at: entry.at,
                stream: entry.stream.clone(),
                level: entry.level.clone(),
                message: capture::redact_text(&entry.message).0,
            })
            .collect();
        sqlx::query(
            "insert into provider_routing_state
                (provider_id, router_id, revision, runtime, policy, clients, lockouts, recent_logs, rolling_starter, updated_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
             on conflict (provider_id) do update set
                router_id = excluded.router_id,
                revision = greatest(provider_routing_state.revision, excluded.revision),
                runtime = excluded.runtime,
                policy = excluded.policy,
                clients = excluded.clients,
                lockouts = excluded.lockouts,
                recent_logs = excluded.recent_logs,
                rolling_starter = excluded.rolling_starter,
                updated_at = now()",
        )
        .bind(provider_id)
        .bind(&routing.router_id)
        .bind(routing.revision)
        .bind(routing.runtime.as_ref().and_then(|r| serde_json::to_value(r).ok()))
        .bind(serde_json::to_value(&routing.policy).unwrap_or_else(|_| json!({})))
        .bind(serde_json::to_value(&routing.clients).unwrap_or_else(|_| json!([])))
        .bind(serde_json::to_value(&routing.lockouts).unwrap_or_else(|_| json!([])))
        .bind(serde_json::to_value(&clean_logs).unwrap_or_else(|_| json!([])))
        .bind(serde_json::to_value(&routing.rolling_starter).unwrap_or_else(|_| json!({})))
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Best-effort quota notification sync into the hub's long-polled notification feed.
    let _ = sync_quota_notifications(hub, provider_id).await;

    Ok(ObservationAck {
        provider_id: provider_id.to_string(),
        accounts_upserted,
        samples_inserted,
        grants_upserted,
        recorded_at: Utc::now(),
    })
}

async fn insert_quota_sample(
    tx: &mut sqlx::PgConnection,
    provider_id: &str,
    account_id: &str,
    win: &QuotaWindow,
    observed_at: DateTime<Utc>,
) -> DResult<()> {
    let kind = serde_json::to_value(win.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "custom".into());
    let unit = serde_json::to_value(win.unit).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "percent".into());
    let cadence = serde_json::to_value(win.cadence).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "unknown".into());
    let status = serde_json::to_value(win.status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "unknown".into());
    let obs_state = serde_json::to_value(win.observation_state)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "never_observed".into());

    let used_pct = win.used_percent.map(|v| v.clamp(0.0, 100.0));
    let rem_pct = win.remaining_percent.map(|v| v.clamp(0.0, 100.0)).or_else(|| used_pct.map(|u| (100.0 - u).clamp(0.0, 100.0)));

    sqlx::query(
        "insert into model_quota_samples
            (provider_id, account_id, window_key, label, kind, unit, used_percent, remaining_percent,
             used_amount, limit_amount, currency, resets_at, window_duration_mins, cadence, scope_model,
             status, observation_state, observed_at, stale, error_reason)
         values ($1, $2, $3, $4, $5, $6, $7, $8,
                 $9, $10, $11, $12, $13, $14, $15,
                 $16, $17, $18, $19, $20)",
    )
    .bind(provider_id)
    .bind(account_id)
    .bind(&win.window_key)
    .bind(&win.label)
    .bind(&kind)
    .bind(&unit)
    .bind(used_pct)
    .bind(rem_pct)
    .bind(win.used_amount)
    .bind(win.limit_amount)
    .bind(&win.currency)
    .bind(win.resets_at)
    .bind(win.window_duration_mins.map(|v| v as i32))
    .bind(&cadence)
    .bind(&win.scope_model)
    .bind(&status)
    .bind(&obs_state)
    .bind(observed_at)
    .bind(win.stale)
    .bind(&win.error_reason)
    .execute(tx)
    .await?;
    Ok(())
}

async fn upsert_reset_grant(tx: &mut sqlx::PgConnection, provider_id: &str, account_id: &str, grant: &ResetGrant) -> DResult<()> {
    sqlx::query(
        "insert into model_reset_grants
            (provider_id, account_id, grant_id, available_count, applicable, checked_at,
             expires_at, scope, cleared_windows, weekly_credit_back, unavailable_reason, updated_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
         on conflict (provider_id, account_id) do update set
            grant_id = excluded.grant_id,
            available_count = excluded.available_count,
            applicable = excluded.applicable,
            checked_at = excluded.checked_at,
            expires_at = excluded.expires_at,
            scope = excluded.scope,
            cleared_windows = excluded.cleared_windows,
            weekly_credit_back = excluded.weekly_credit_back,
            unavailable_reason = excluded.unavailable_reason,
            updated_at = now()",
    )
    .bind(provider_id)
    .bind(account_id)
    .bind(&grant.grant_id)
    .bind(grant.available_count as i32)
    .bind(grant.applicable)
    .bind(grant.checked_at)
    .bind(grant.expires_at)
    .bind(&grant.scope)
    .bind(serde_json::to_value(&grant.cleared_windows).unwrap_or_else(|_| json!([])))
    .bind(grant.weekly_credit_back)
    .bind(&grant.unavailable_reason)
    .execute(tx)
    .await?;
    Ok(())
}

pub(super) async fn enforce_collector_source(hub: &Hub, actor: &Actor, host: &str, unix_user: &str) -> DResult<()> {
    if actor.principal.role == contracts::Role::Operator {
        return Ok(());
    }
    let has_any_grant: bool = sqlx::query_scalar("select exists(select 1 from model_source_grants where principal_name = $1)")
        .bind(&actor.principal.name)
        .fetch_one(&hub.db)
        .await?;
    if !has_any_grant {
        return Ok(());
    }
    let exists: bool =
        sqlx::query_scalar("select exists(select 1 from model_source_grants where principal_name = $1 and host = $2 and unix_user = $3)")
            .bind(&actor.principal.name)
            .bind(host)
            .bind(unix_user)
            .fetch_one(&hub.db)
            .await?;
    if !exists {
        return Err(DomainError::Forbidden(format!("collector {} is not granted scope for {unix_user}@{host}", actor.principal.name)));
    }
    Ok(())
}

pub(super) async fn enforce_collector_scope(hub: &Hub, actor: &Actor, host: &str, unix_user: &str, provider_id: &str) -> DResult<()> {
    if actor.principal.role == contracts::Role::Operator {
        return Ok(());
    }
    let grant: Option<(Value,)> =
        sqlx::query_as("select provider_ids from model_source_grants where principal_name = $1 and host = $2 and unix_user = $3")
            .bind(&actor.principal.name)
            .bind(host)
            .bind(unix_user)
            .fetch_optional(&hub.db)
            .await?;
    // If explicit source grants exist for this collector principal, enforce them strictly.
    let has_any_grant: bool = sqlx::query_scalar("select exists(select 1 from model_source_grants where principal_name = $1)")
        .bind(&actor.principal.name)
        .fetch_one(&hub.db)
        .await?;
    if !has_any_grant {
        return Ok(());
    }
    let Some((providers_json,)) = grant else {
        return Err(DomainError::Forbidden(format!("collector {} is not granted scope for {unix_user}@{host}", actor.principal.name)));
    };
    let allowed: Vec<String> = serde_json::from_value(providers_json).unwrap_or_default();
    if !allowed.is_empty() && !allowed.iter().any(|p| p == provider_id || p == "*") {
        return Err(DomainError::Forbidden(format!("collector {} is not granted provider {provider_id}", actor.principal.name)));
    }
    Ok(())
}

// ── Read Models & Pool / Forecast Computations ──────────────────────────────

pub async fn list_providers(hub: &Hub, actor: &Actor) -> DResult<Vec<ProviderSummary>> {
    actor.require(ANY_READER)?;
    let descriptors = load_descriptors(hub).await?;
    let mut out = Vec::with_capacity(descriptors.len());
    for desc in descriptors {
        out.push(build_provider_summary(hub, desc).await?);
    }
    Ok(out)
}

pub async fn get_provider(hub: &Hub, actor: &Actor, provider_id: &str) -> DResult<ProviderSummary> {
    actor.require(ANY_READER)?;
    let desc = load_descriptor(hub, provider_id).await?;
    build_provider_summary(hub, desc).await
}

pub async fn list_accounts(hub: &Hub, actor: &Actor, provider_id: &str) -> DResult<Vec<ModelAccount>> {
    actor.require(ANY_READER)?;
    let _ = load_descriptor(hub, provider_id).await?;
    load_accounts_with_latest_windows(&hub.db, provider_id).await
}

pub async fn list_models(hub: &Hub, actor: &Actor, provider_id: &str) -> DResult<Vec<DiscoveredModel>> {
    actor.require(ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(m) from (
            select model_id as id, display_name, family, provider_id, group_id, context_window, supports_thinking, discovered_at
            from model_catalog where provider_id = $1 order by model_id
         ) m",
    )
    .bind(provider_id)
    .fetch_all(&hub.db)
    .await?;
    crate::decode(rows)
}

pub async fn list_key_groups(hub: &Hub, actor: &Actor, provider_id: &str) -> DResult<Vec<KeyGroup>> {
    actor.require(ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(g) from (
            select id, provider_id, family, name, base_url, enabled, request_identity,
                   keys, configured_models, discovered_models, aliases, updated_at
            from model_key_groups where provider_id = $1 order by name, id
         ) g",
    )
    .bind(provider_id)
    .fetch_all(&hub.db)
    .await?;
    crate::decode(rows)
}

pub async fn get_routing(hub: &Hub, actor: &Actor, provider_id: &str) -> DResult<RoutingView> {
    actor.require(ANY_READER)?;
    let row: Option<Value> = sqlx::query_scalar(
        "select to_jsonb(r) from (
            select provider_id, router_id, revision, runtime, policy, clients, lockouts, recent_logs, rolling_starter
            from provider_routing_state where provider_id = $1
         ) r",
    )
    .bind(provider_id)
    .fetch_optional(&hub.db)
    .await?;
    if let Some(val) = row {
        return crate::decode_one(val);
    }
    Ok(RoutingView {
        provider_id: provider_id.to_string(),
        router_id: None,
        revision: 0,
        runtime: None,
        policy: RoutingPolicy::default(),
        clients: Vec::new(),
        lockouts: Vec::new(),
        recent_logs: Vec::new(),
        rolling_starter: RollingWindowStarterConfig::default(),
    })
}

pub async fn get_logs(hub: &Hub, actor: &Actor, provider_id: &str) -> DResult<Vec<ProxyLogEntry>> {
    actor.require(OPERATOR)?;
    let view = get_routing(hub, actor, provider_id).await?;
    Ok(view.recent_logs)
}

pub async fn quota_report(hub: &Hub, actor: &Actor, provider_id: &str, query: QuotaQuery) -> DResult<ProviderQuotaReport> {
    actor.require(ANY_READER)?;
    let desc = load_descriptor(hub, provider_id).await?;
    let accounts = load_accounts_with_latest_windows(&hub.db, provider_id).await?;
    let range_hours = query.range_hours.unwrap_or(48).clamp(1, 24 * 35);
    let lookback_hours = query.lookback_hours.unwrap_or(6).clamp(1, 168);
    let horizon_hours = query.horizon_hours.unwrap_or(12).clamp(1, 168);
    let since = Utc::now() - Duration::hours(range_hours as i64);

    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(s) from (
            select id, provider_id, account_id, window_key, label, kind, unit,
                   used_percent, remaining_percent, used_amount, limit_amount,
                   resets_at, window_duration_mins, cadence, status, observation_state,
                   observed_at, recorded_at, stale, error_reason
            from model_quota_samples
            where provider_id = $1
              and observed_at >= $2
              and ($3::text is null or account_id = $3)
              and ($4::text is null or window_key = $4)
            order by observed_at asc, id asc
            limit 4000
         ) s",
    )
    .bind(provider_id)
    .bind(since)
    .bind(&query.account_id)
    .bind(&query.window_key)
    .fetch_all(&hub.db)
    .await?;
    let samples: Vec<QuotaSample> = crate::decode(rows)?;

    let past_windows = compute_past_windows(&samples);
    let forecasts = compute_forecasts(&samples, &accounts, lookback_hours, horizon_hours, Utc::now());
    let pools = compute_pools(provider_id, &accounts, &forecasts);

    Ok(ProviderQuotaReport { provider: desc, accounts, pools, samples, past_windows, forecasts, as_of: Utc::now() })
}

async fn load_descriptors(hub: &Hub) -> DResult<Vec<ProviderDescriptor>> {
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(p) from (
            select id, label, family, mode, verification, evidence_kinds, capabilities,
                   router_id, health, status_message, last_observed_at
            from model_provider_instances
            order by id
         ) p",
    )
    .fetch_all(&hub.db)
    .await?;
    let mut by_id: BTreeMap<String, ProviderDescriptor> = BTreeMap::new();
    for row in rows {
        let mut d: ProviderDescriptor = crate::decode_one(row)?;
        if let Some(live) = hub.providers.get(&d.id) {
            let live_desc = live.descriptor();
            d.capabilities.extend(live_desc.capabilities);
            d.evidence_kinds.extend(live_desc.evidence_kinds);
            d.mode = live_desc.mode;
            d.verification = live_desc.verification;
        }
        by_id.insert(d.id.clone(), d);
    }
    for (id, provider) in hub.providers.iter() {
        by_id.entry(id.clone()).or_insert_with(|| provider.descriptor());
    }
    Ok(by_id.into_values().collect())
}

async fn load_descriptor(hub: &Hub, provider_id: &str) -> DResult<ProviderDescriptor> {
    let all = load_descriptors(hub).await?;
    all.into_iter().find(|d| d.id == provider_id).ok_or(DomainError::NotFound("model provider"))
}

async fn build_provider_summary(hub: &Hub, descriptor: ProviderDescriptor) -> DResult<ProviderSummary> {
    let accounts = load_accounts_with_latest_windows(&hub.db, &descriptor.id).await?;
    let enabled_accounts = accounts.iter().filter(|a| a.enabled).count();
    let active_account_id = accounts.iter().find(|a| a.is_active).map(|a| a.id.clone());
    let preferred_account_id = accounts.iter().find(|a| a.is_preferred).map(|a| a.id.clone());
    let next_pick_account_id = accounts.iter().find(|a| a.is_next_pick).map(|a| a.id.clone());
    let pools = compute_pools(&descriptor.id, &accounts, &[]);
    Ok(ProviderSummary {
        descriptor,
        account_count: accounts.len(),
        enabled_accounts,
        active_account_id,
        preferred_account_id,
        next_pick_account_id,
        pools,
        accounts,
    })
}

async fn load_accounts_with_latest_windows(db: &sqlx::PgPool, provider_id: &str) -> DResult<Vec<ModelAccount>> {
    let acct_rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(a) from (
            select id, provider_id, source_account_id, display_name, email, masked_email, note, plan,
                   tier_multiplier, enabled, priority, weight, cooldown_until, status_message,
                   is_active, is_preferred, is_next_pick, enrollment_state, host, unix_user,
                   credential_fingerprint, last_used_at, last_observed_at, capabilities,
                   success_count, failure_count, rolling_starter
            from model_accounts
            where provider_id = $1
            order by priority desc, id asc
         ) a",
    )
    .bind(provider_id)
    .fetch_all(db)
    .await?;

    let win_rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(w) from (
            select distinct on (account_id, window_key)
                account_id, window_key, label, kind, unit, used_percent, remaining_percent,
                used_amount, limit_amount, currency, resets_at, window_duration_mins,
                cadence, scope_model, status, observation_state, observed_at, stale, error_reason
            from model_quota_samples
            where provider_id = $1
            order by account_id, window_key, observed_at desc, id desc
         ) w",
    )
    .bind(provider_id)
    .fetch_all(db)
    .await?;

    let mut windows_by_acct: BTreeMap<String, Vec<QuotaWindow>> = BTreeMap::new();
    for row in win_rows {
        let acct_id = row.get("account_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let win: QuotaWindow = crate::decode_one(row)?;
        windows_by_acct.entry(acct_id).or_default().push(win);
    }

    let grant_rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(g) from (
            select account_id, grant_id, available_count, applicable, checked_at, expires_at,
                   scope, cleared_windows, weekly_credit_back, unavailable_reason
            from model_reset_grants where provider_id = $1
         ) g",
    )
    .bind(provider_id)
    .fetch_all(db)
    .await?;
    let mut grants_by_acct: BTreeMap<String, ResetGrant> = BTreeMap::new();
    for row in grant_rows {
        let acct_id = row.get("account_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let grant: ResetGrant = crate::decode_one(row)?;
        grants_by_acct.insert(acct_id, grant);
    }

    let now = Utc::now();
    let mut out = Vec::with_capacity(acct_rows.len());
    for row in acct_rows {
        let mut acct: ModelAccount = crate::decode_one(row)?;
        acct.windows = windows_by_acct.remove(&acct.id).unwrap_or_default();
        acct.reset_grant = grants_by_acct.remove(&acct.id);
        let has_err = acct.enrollment_state == EnrollmentState::Error
            || acct.windows.iter().any(|w| matches!(w.observation_state, QuotaObservationState::ProbeFailed));
        acct.status = AccountStatus::resolve(acct.enabled, acct.cooldown_until, now, acct.is_next_pick, has_err);
        out.push(acct);
    }
    Ok(out)
}

/// Combine compatible windows across distinct eligible enabled accounts.
/// Never averages missing/unobserved accounts as zero and never treats routing
/// weight as quota capacity (uses plan `tier_multiplier` when available).
pub fn compute_pools(provider_id: &str, accounts: &[ModelAccount], forecasts: &[QuotaForecast]) -> Vec<PoolWindowSummary> {
    let eligible: Vec<&ModelAccount> = accounts.iter().filter(|a| a.enabled).collect();
    // Deduplicate logical entitlements by credential_fingerprint when present.
    let mut seen_fp: BTreeSet<String> = BTreeSet::new();
    let mut distinct_eligible: Vec<&ModelAccount> = Vec::new();
    for acct in eligible {
        if let Some(fp) = &acct.credential_fingerprint
            && !seen_fp.insert(fp.clone())
        {
            continue;
        }
        distinct_eligible.push(acct);
    }

    let mut by_window: BTreeMap<String, Vec<(&ModelAccount, &QuotaWindow)>> = BTreeMap::new();
    for acct in &distinct_eligible {
        for win in &acct.windows {
            if win.unit != QuotaUnit::Percent {
                continue;
            }
            by_window.entry(win.window_key.clone()).or_default().push((acct, win));
        }
    }

    let mut pools = Vec::new();
    for (window_key, entries) in by_window {
        let Some((_, first)) = entries.first() else {
            continue;
        };
        let mut weighted_used = 0.0;
        let mut total_capacity = 0.0;
        let mut included = 0usize;
        let mut any_tier_weight = false;
        let mut next_reset: Option<DateTime<Utc>> = None;

        for (acct, win) in &entries {
            let Some(used) = win.used_percent else {
                continue;
            };
            if !matches!(win.observation_state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood) {
                continue;
            }
            let cap = acct.tier_multiplier.filter(|m| m.is_finite() && *m > 0.0).unwrap_or(1.0);
            if acct.tier_multiplier.is_some() {
                any_tier_weight = true;
            }
            weighted_used += used * cap;
            total_capacity += cap;
            included += 1;
            if let Some(r) = win.resets_at {
                next_reset = Some(match next_reset {
                    Some(prev) => prev.min(r),
                    None => r,
                });
            }
        }

        let (used_pct, rem_pct) = if total_capacity > 0.0 {
            let u = (weighted_used / total_capacity).clamp(0.0, 100.0);
            (Some(u), Some((100.0 - u).clamp(0.0, 100.0)))
        } else {
            (None, None)
        };

        let forecast = forecasts.iter().find(|f| f.window_key == window_key && f.account_id.is_none()).cloned();
        pools.push(PoolWindowSummary {
            provider_id: provider_id.to_string(),
            window_key,
            label: first.label.clone(),
            kind: first.kind,
            eligible_accounts: distinct_eligible.len(),
            included_accounts: included,
            weighted_used_percent: used_pct,
            weighted_remaining_percent: rem_pct,
            capacity_basis: if any_tier_weight { "plan_tier_multiplier".into() } else { "equal_entitlements".into() },
            next_reset_at: next_reset,
            forecast,
            is_estimate: true,
        });
    }
    pools
}

/// Partition ordered quota samples into past completed windows by detecting
/// reset boundaries (either `resets_at` advancing or a sharp drop in `used_percent`).
pub fn compute_past_windows(samples: &[QuotaSample]) -> Vec<PastWindowSummary> {
    let mut grouped: BTreeMap<(String, String), Vec<&QuotaSample>> = BTreeMap::new();
    for s in samples {
        if s.used_percent.is_some() {
            grouped.entry((s.account_id.clone(), s.window_key.clone())).or_default().push(s);
        }
    }

    let mut out = Vec::new();
    for ((account_id, window_key), series) in grouped {
        if series.len() < 2 {
            continue;
        }
        let mut segment: Vec<&QuotaSample> = vec![series[0]];
        for pair in series.windows(2) {
            let prev = pair[0];
            let curr = pair[1];
            let reset_advanced = match (prev.resets_at, curr.resets_at) {
                (Some(a), Some(b)) => b > a + Duration::minutes(5),
                _ => false,
            };
            let usage_dropped = match (prev.used_percent, curr.used_percent) {
                (Some(a), Some(b)) => a >= 10.0 && b + 15.0 < a,
                _ => false,
            };
            if reset_advanced || usage_dropped {
                if let Some(summary) = summarize_segment(&account_id, &window_key, &segment, curr.observed_at) {
                    out.push(summary);
                }
                segment.clear();
            }
            segment.push(curr);
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.ended_at));
    out
}

fn summarize_segment(account_id: &str, window_key: &str, segment: &[&QuotaSample], next_seen: DateTime<Utc>) -> Option<PastWindowSummary> {
    let first = *segment.first()?;
    let last = *segment.last()?;
    let peak = segment.iter().filter_map(|s| s.used_percent).fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
    let exhausted_at = segment.iter().find(|s| s.used_percent.is_some_and(|u| u >= 99.5)).map(|s| s.observed_at);
    let early_replaced = last.resets_at.is_some_and(|r| next_seen + Duration::minutes(10) < r);
    Some(PastWindowSummary {
        account_id: account_id.to_string(),
        window_key: window_key.to_string(),
        label: first.label.clone(),
        started_at: first.observed_at,
        ended_at: last.resets_at.unwrap_or(last.observed_at),
        peak_used_percent: peak,
        min_remaining_percent: peak.map(|p| (100.0 - p).clamp(0.0, 100.0)),
        exhausted_at,
        early_replaced,
        first_observed_at: first.observed_at,
        last_observed_at: last.observed_at,
        sample_count: segment.len() as u64,
    })
}

/// Reset-aware drain-rate estimation and forward projection over `horizon_hours`.
pub fn compute_forecasts(
    samples: &[QuotaSample],
    accounts: &[ModelAccount],
    lookback_hours: u32,
    horizon_hours: u32,
    now: DateTime<Utc>,
) -> Vec<QuotaForecast> {
    let cutoff = now - Duration::hours(lookback_hours as i64);
    let mut by_acct_win: BTreeMap<(String, String), Vec<&QuotaSample>> = BTreeMap::new();
    for s in samples {
        if s.observed_at >= cutoff && s.used_percent.is_some() {
            by_acct_win.entry((s.account_id.clone(), s.window_key.clone())).or_default().push(s);
        }
    }

    let mut forecasts = Vec::new();
    type PoolRateSample = (f64, f64, Option<DateTime<Utc>>);
    let mut pool_rates: BTreeMap<String, Vec<PoolRateSample>> = BTreeMap::new();

    for ((account_id, window_key), series) in by_acct_win {
        let Some(last) = series.last().copied() else {
            continue;
        };
        let current_used = last.used_percent.unwrap_or(0.0);
        // Accumulate positive delta segments only (reset-aware: drop negative drops across resets).
        let mut positive_delta = 0.0;
        let mut elapsed_hours = 0.0;
        for pair in series.windows(2) {
            let dt_h = (pair[1].observed_at - pair[0].observed_at).num_seconds().max(0) as f64 / 3600.0;
            if dt_h <= 0.0 || dt_h > 6.0 {
                // Skip observation gaps wider than 6h so gaps don't flatten burn rate.
                continue;
            }
            let du = pair[1].used_percent.unwrap_or(0.0) - pair[0].used_percent.unwrap_or(0.0);
            if du >= 0.0 {
                positive_delta += du;
                elapsed_hours += dt_h;
            }
        }
        let drain_per_hour = if elapsed_hours > 0.01 { (positive_delta / elapsed_hours).max(0.0) } else { 0.0 };
        let resets_at = accounts
            .iter()
            .find(|a| a.id == account_id)
            .and_then(|a| a.windows.iter().find(|w| w.window_key == window_key))
            .and_then(|w| w.resets_at)
            .or(last.resets_at);

        pool_rates.entry(window_key.clone()).or_default().push((current_used, drain_per_hour, resets_at));
        forecasts.push(build_forecast(
            Some(account_id),
            window_key,
            current_used,
            drain_per_hour,
            resets_at,
            lookback_hours,
            horizon_hours,
            now,
        ));
    }

    // Aggregate pool forecast per window_key.
    for (window_key, entries) in pool_rates {
        let n = entries.len() as f64;
        if n <= 0.0 {
            continue;
        }
        let avg_used = entries.iter().map(|(u, _, _)| *u).sum::<f64>() / n;
        let avg_drain = entries.iter().map(|(_, r, _)| *r).sum::<f64>() / n;
        let next_reset = entries.iter().filter_map(|(_, _, r)| *r).min();
        forecasts.push(build_forecast(None, window_key, avg_used, avg_drain, next_reset, lookback_hours, horizon_hours, now));
    }

    forecasts
}

#[allow(clippy::too_many_arguments)]
fn build_forecast(
    account_id: Option<String>,
    window_key: String,
    current_used: f64,
    drain_per_hour: f64,
    resets_at: Option<DateTime<Utc>>,
    lookback_hours: u32,
    horizon_hours: u32,
    now: DateTime<Utc>,
) -> QuotaForecast {
    let remaining = (100.0 - current_used).max(0.0);
    let projected_exhaustion_at = if drain_per_hour > 0.001 && remaining > 0.0 {
        let hours_left = remaining / drain_per_hour;
        Some(now + Duration::seconds((hours_left * 3600.0) as i64))
    } else if remaining <= 0.0 {
        Some(now)
    } else {
        None
    };
    let resets_before_exhaustion = match (resets_at, projected_exhaustion_at) {
        (Some(r), Some(e)) => r < e,
        (Some(_), None) => true,
        _ => false,
    };

    let steps = horizon_hours.clamp(2, 24) as usize;
    let step_secs = ((horizon_hours as i64) * 3600) / (steps as i64);
    let mut points = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let at = now + Duration::seconds(step_secs * (i as i64));
        let dt_h = (at - now).num_seconds() as f64 / 3600.0;
        let used = if resets_at.is_some_and(|r| at >= r) {
            // Post-reset projection restarts cleanly from 0; predicted reset is still an estimate.
            let post_h = (at - resets_at.unwrap()).num_seconds().max(0) as f64 / 3600.0;
            (post_h * drain_per_hour).clamp(0.0, 100.0)
        } else {
            (current_used + dt_h * drain_per_hour).clamp(0.0, 100.0)
        };
        points.push(ForecastPoint { at, used_percent: used, remaining_percent: (100.0 - used).clamp(0.0, 100.0) });
    }

    QuotaForecast {
        window_key,
        account_id,
        drain_pct_per_hour: drain_per_hour,
        projected_exhaustion_at,
        resets_before_exhaustion,
        lookback_hours,
        horizon_hours,
        points,
        is_estimate: true,
    }
}

// ── Capability-Gated Typed Operations ───────────────────────────────────────

pub async fn submit_operation(hub: &Hub, actor: &Actor, provider_id: &str, input: SubmitOperationIn) -> DResult<ProviderOperation> {
    actor.require(OPERATOR)?;
    if input.idempotency_key.trim().is_empty() {
        return Err(invalid("missing_idempotency_key", "idempotency_key is required"));
    }
    let desc = load_descriptor(hub, provider_id).await?;
    let required = input.command.required_capability();
    if !desc.capabilities.contains(&required) {
        return Err(invalid("capability_unsupported", format!("provider {provider_id} does not support {:?}", required)));
    }

    let cmd_json = serde_json::to_value(&input.command).map_err(|e| DomainError::Internal(e.to_string()))?;
    let req_hash = hash::sha256_hex(
        serde_json::to_vec(&json!({
            "command": cmd_json,
            "target_account_id": input.target_account_id,
            "target_host": input.target_host,
            "target_user": input.target_user,
            "expected_revision": input.expected_revision,
        }))
        .unwrap_or_default()
        .as_slice(),
    );

    // Idempotency check: same key + same payload returns existing intent; changed payload conflicts.
    if let Some((existing_id, existing_hash)) = sqlx::query_as::<_, (Uuid, String)>(
        "select id, request_sha256 from model_operations where provider_id = $1 and idempotency_key = $2",
    )
    .bind(provider_id)
    .bind(&input.idempotency_key)
    .fetch_optional(&hub.db)
    .await?
    {
        if existing_hash != req_hash {
            return Err(conflict("idempotency_conflict", "idempotency_key already used with a different command payload"));
        }
        return get_operation(hub, actor, existing_id).await;
    }

    // Optimistic revision check for routing updates when caller supplies expected_revision.
    if let Some(expected) = input.expected_revision {
        let current_rev: Option<i64> = sqlx::query_scalar("select revision from provider_routing_state where provider_id = $1")
            .bind(provider_id)
            .fetch_optional(&hub.db)
            .await?;
        if let Some(rev) = current_rev
            && rev != expected
        {
            return Err(conflict("stale_revision", format!("expected revision {expected}, current is {rev}")));
        }
    }

    let op_id = Uuid::new_v4();
    let command_kind = input.command.kind_name();
    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "insert into model_operations
            (id, provider_id, actor, idempotency_key, request_sha256, command_kind, command,
             target_account_id, target_host, target_user, expected_revision, status)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'pending')",
    )
    .bind(op_id)
    .bind(provider_id)
    .bind(actor.name())
    .bind(&input.idempotency_key)
    .bind(&req_hash)
    .bind(command_kind)
    .bind(&cmd_json)
    .bind(&input.target_account_id)
    .bind(&input.target_host)
    .bind(&input.target_user)
    .bind(input.expected_revision)
    .execute(&mut *tx)
    .await?;

    // Audit metadata only — never log raw credentials or bodies.
    audit(
        &mut tx,
        actor,
        "providers.operation.submitted",
        "model_provider",
        provider_id,
        json!({
            "operation_id": op_id,
            "command_kind": command_kind,
            "target_account_id": input.target_account_id,
            "target_host": input.target_host,
        }),
    )
    .await?;
    tx.commit().await?;

    // Apply immediate local metadata mutations when the hub itself owns the record
    // (notes, enable/disable, priority/weight, routing policy) or dispatch to registered provider.
    if let Some(provider) = hub.providers.get(provider_id) {
        let intent = ProviderIntent {
            operation_id: op_id,
            provider_id: provider_id.to_string(),
            actor: actor.name(),
            idempotency_key: input.idempotency_key.clone(),
            target_account_id: input.target_account_id.clone(),
            target_host: input.target_host.clone(),
            target_user: input.target_user.clone(),
            expected_revision: input.expected_revision,
        };
        let facets = provider.facets();
        let dispatch_result = match &input.command {
            ProviderCommand::Quota(c) => match facets.quota {
                Some(f) => f.submit(&intent, c).await,
                None => Err(crate::ports::PortError::Unsupported("quota facet unavailable".into())),
            },
            ProviderCommand::Account(c) => match facets.accounts {
                Some(f) => f.submit(&intent, c).await,
                None => Err(crate::ports::PortError::Unsupported("accounts facet unavailable".into())),
            },
            ProviderCommand::Credential(c) => match facets.credentials {
                Some(f) => f.submit(&intent, c).await,
                None => Err(crate::ports::PortError::Unsupported("credentials facet unavailable".into())),
            },
            ProviderCommand::KeyGroup(c) => match facets.key_groups {
                Some(f) => f.submit(&intent, c).await,
                None => Err(crate::ports::PortError::Unsupported("key_groups facet unavailable".into())),
            },
            ProviderCommand::Model(c) => match facets.models {
                Some(f) => f.submit(&intent, c).await,
                None => Err(crate::ports::PortError::Unsupported("models facet unavailable".into())),
            },
            ProviderCommand::Routing(c) => match facets.routing {
                Some(f) => f.submit(&intent, c).await,
                None => Err(crate::ports::PortError::Unsupported("routing facet unavailable".into())),
            },
            ProviderCommand::Probe(c) => match facets.probes {
                Some(f) => f.submit(&intent, c).await,
                None => Err(crate::ports::PortError::Unsupported("probes facet unavailable".into())),
            },
        };

        match dispatch_result {
            Ok(ProviderDispatch::Completed { receipt }) => {
                let (safe_receipt, _) = capture::redact_json(&receipt);
                apply_hub_side_effects(hub, provider_id, &input.command).await?;
                update_operation_state(&hub.db, op_id, OperationStatus::Succeeded, Some(safe_receipt), None).await?;
                if let Ok(Some(batch)) = provider.collect().await {
                    let _ = ingest_observations(hub, &internal_operator("provider-refresh"), provider_id, batch).await;
                }
            }
            Ok(ProviderDispatch::Accepted { receipt }) => {
                let safe = receipt.map(|r| capture::redact_json(&r).0);
                update_operation_state(&hub.db, op_id, OperationStatus::Dispatched, safe, None).await?;
            }
            Ok(ProviderDispatch::AwaitingOperator { message, receipt }) => {
                let mut val = receipt.unwrap_or_else(|| json!({}));
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("message".into(), Value::String(message));
                }
                let (safe, _) = capture::redact_json(&val);
                update_operation_state(&hub.db, op_id, OperationStatus::AwaitingOperator, Some(safe), None).await?;
            }
            Err(crate::ports::PortError::Ambiguous(msg)) => {
                update_operation_state(&hub.db, op_id, OperationStatus::Ambiguous, None, Some(capture::redact_text(&msg).0)).await?;
            }
            Err(err) => {
                update_operation_state(&hub.db, op_id, OperationStatus::Failed, None, Some(capture::redact_text(&err.to_string()).0))
                    .await?;
            }
        }
    }

    get_operation(hub, actor, op_id).await
}

/// Apply hub-persisted metadata reflections for completed commands so DB views stay immediate.
async fn apply_hub_side_effects(hub: &Hub, provider_id: &str, command: &ProviderCommand) -> DResult<()> {
    match command {
        ProviderCommand::Account(AccountCommand::SetEnabled { account_id, enabled }) => {
            sqlx::query("update model_accounts set enabled = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(enabled)
                .execute(&hub.db)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SetPriority { account_id, priority }) => {
            sqlx::query("update model_accounts set priority = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(priority)
                .execute(&hub.db)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SetWeight { account_id, weight }) => {
            sqlx::query("update model_accounts set weight = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(*weight as i32)
                .execute(&hub.db)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SetNote { account_id, note }) => {
            sqlx::query("update model_accounts set note = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(note)
                .execute(&hub.db)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SelectLocalAccount { account_id, .. })
        | ProviderCommand::Account(AccountCommand::SelectPersistentAccount { account_id, .. }) => {
            sqlx::query("update model_accounts set is_active = (id = $2), updated_at = now() where provider_id = $1")
                .bind(provider_id)
                .bind(account_id)
                .execute(&hub.db)
                .await?;
        }
        ProviderCommand::Routing(RoutingCommand::SetPoolOverride { account_id }) => {
            sqlx::query(
                "update model_accounts set is_preferred = ($2::text is not null and id = $2), updated_at = now() where provider_id = $1",
            )
            .bind(provider_id)
            .bind(account_id)
            .execute(&hub.db)
            .await?;
            sqlx::query(
                "update provider_routing_state
                 set policy = jsonb_set(policy, '{preferred_account_id}', coalesce(to_jsonb($2::text), 'null'::jsonb), true),
                     revision = revision + 1,
                     updated_at = now()
                 where provider_id = $1",
            )
            .bind(provider_id)
            .bind(account_id)
            .execute(&hub.db)
            .await?;
        }
        ProviderCommand::Routing(RoutingCommand::SetRoutePolicy { policy }) => {
            sqlx::query(
                "insert into provider_routing_state (provider_id, policy, revision, updated_at)
                 values ($1, $2, 1, now())
                 on conflict (provider_id) do update set
                    policy = excluded.policy,
                    revision = provider_routing_state.revision + 1,
                    updated_at = now()",
            )
            .bind(provider_id)
            .bind(serde_json::to_value(policy).unwrap_or_else(|_| json!({})))
            .execute(&hub.db)
            .await?;
        }
        ProviderCommand::Quota(QuotaCommand::ConfigureRollingStarter { config }) => {
            sqlx::query(
                "insert into provider_routing_state (provider_id, rolling_starter, revision, updated_at)
                 values ($1, $2, 1, now())
                 on conflict (provider_id) do update set
                    rolling_starter = excluded.rolling_starter,
                    revision = provider_routing_state.revision + 1,
                    updated_at = now()",
            )
            .bind(provider_id)
            .bind(serde_json::to_value(config).unwrap_or_else(|_| json!({})))
            .execute(&hub.db)
            .await?;
        }
        ProviderCommand::Probe(ProbeCommand::ConfigureCanarySchedule { schedule }) => {
            sqlx::query(
                "insert into settings (key, value, updated_at) values ('provider_canary_schedule', $1, now())
                 on conflict (key) do update set value = excluded.value, updated_at = now()",
            )
            .bind(serde_json::to_value(schedule).unwrap_or_else(|_| json!({})))
            .execute(&hub.db)
            .await?;
        }
        ProviderCommand::KeyGroup(KeyGroupCommand::DeleteGroup { group_id }) => {
            sqlx::query("delete from model_key_groups where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(group_id)
                .execute(&hub.db)
                .await?;
        }
        ProviderCommand::KeyGroup(KeyGroupCommand::SetGroupEnabled { group_id, enabled }) => {
            sqlx::query("update model_key_groups set enabled = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(group_id)
                .bind(enabled)
                .execute(&hub.db)
                .await?;
        }
        ProviderCommand::KeyGroup(KeyGroupCommand::RemoveKey { group_id, key_id }) => {
            sqlx::query(
                "update model_key_groups
                 set keys = coalesce((select jsonb_agg(k) from jsonb_array_elements(keys) k where k->>'id' <> $3), '[]'::jsonb),
                     updated_at = now()
                 where provider_id = $1 and id = $2",
            )
            .bind(provider_id)
            .bind(group_id)
            .bind(key_id)
            .execute(&hub.db)
            .await?;
        }
        ProviderCommand::KeyGroup(KeyGroupCommand::ToggleKey { group_id, key_id, enabled }) => {
            sqlx::query(
                "update model_key_groups
                 set keys = coalesce((
                     select jsonb_agg(case when k->>'id' = $3 then jsonb_set(k, '{enabled}', to_jsonb($4::bool), true) else k end)
                     from jsonb_array_elements(keys) k
                 ), '[]'::jsonb),
                 updated_at = now()
                 where provider_id = $1 and id = $2",
            )
            .bind(provider_id)
            .bind(group_id)
            .bind(key_id)
            .bind(enabled)
            .execute(&hub.db)
            .await?;
        }
        ProviderCommand::Credential(CredentialCommand::RemoveLogin { account_id }) => {
            sqlx::query("delete from model_reset_grants where provider_id = $1 and account_id = $2")
                .bind(provider_id)
                .bind(account_id)
                .execute(&hub.db)
                .await?;
            sqlx::query("delete from model_accounts where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .execute(&hub.db)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn update_operation_state(
    db: &sqlx::PgPool,
    op_id: Uuid,
    status: OperationStatus,
    receipt: Option<Value>,
    error: Option<String>,
) -> DResult<()> {
    let status_str = serde_json::to_value(status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "failed".into());
    sqlx::query("update model_operations set status = $2, receipt = coalesce($3, receipt), error = $4, updated_at = now() where id = $1")
        .bind(op_id)
        .bind(&status_str)
        .bind(&receipt)
        .bind(&error)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn get_operation(hub: &Hub, actor: &Actor, op_id: Uuid) -> DResult<ProviderOperation> {
    actor.require(OPERATOR_OR_COLLECTOR)?;
    let row: Option<Value> = sqlx::query_scalar(
        "select to_jsonb(o) from (
            select id, provider_id, actor, idempotency_key, command_kind, command,
                   target_account_id, target_host, target_user, expected_revision,
                   status, receipt, error, created_at, updated_at
            from model_operations where id = $1
         ) o",
    )
    .bind(op_id)
    .fetch_optional(&hub.db)
    .await?;
    let row = row.ok_or(DomainError::NotFound("provider operation"))?;
    crate::decode_one(row)
}

/// Claim pending operations destined for a collector's `(host, unix_user)`.
/// Dispatched operations hold a 5-minute lease so overlapping collectors cannot
/// double-claim in-flight work.
pub async fn claim_operations(hub: &Hub, actor: &Actor, input: ClaimOperationsIn) -> DResult<Vec<ProviderOperation>> {
    actor.require(OPERATOR_OR_COLLECTOR)?;
    let effective_providers = enforce_collector_claim_scope(hub, actor, &input.host, &input.unix_user, &input.provider_ids).await?;
    let limit = input.limit.clamp(1, 50) as i64;
    let rows: Vec<Value> = sqlx::query_scalar(
        "with picked as (
            select id from model_operations
            where (
                status = 'pending'
                or (status = 'dispatched' and (claimed_at is null or claimed_at < now() - interval '5 minutes'))
              )
              and (target_host is null or target_host = $1)
              and (target_user is null or target_user = $2)
              and (cardinality($3::text[]) = 0 or provider_id = any($3))
            order by created_at asc
            limit $4
            for update skip locked
         )
         update model_operations o
         set status = 'dispatched', claimed_by = $5, claimed_at = now(), updated_at = now()
         from picked
         where o.id = picked.id
         returning to_jsonb(o)",
    )
    .bind(&input.host)
    .bind(&input.unix_user)
    .bind(&effective_providers)
    .bind(limit)
    .bind(actor.name())
    .fetch_all(&hub.db)
    .await?;
    crate::decode(rows)
}

async fn enforce_collector_claim_scope(
    hub: &Hub,
    actor: &Actor,
    host: &str,
    unix_user: &str,
    requested_providers: &[String],
) -> DResult<Vec<String>> {
    if actor.principal.role == contracts::Role::Operator {
        return Ok(requested_providers.to_vec());
    }
    let has_any_grant: bool = sqlx::query_scalar("select exists(select 1 from model_source_grants where principal_name = $1)")
        .bind(&actor.principal.name)
        .fetch_one(&hub.db)
        .await?;
    if !has_any_grant {
        return Ok(requested_providers.to_vec());
    }
    let grant: Option<(Value, bool)> = sqlx::query_as(
        "select provider_ids, can_claim_operations from model_source_grants
         where principal_name = $1 and host = $2 and unix_user = $3",
    )
    .bind(&actor.principal.name)
    .bind(host)
    .bind(unix_user)
    .fetch_optional(&hub.db)
    .await?;
    let Some((providers_json, can_claim)) = grant else {
        return Err(DomainError::Forbidden(format!("collector {} is not granted scope for {unix_user}@{host}", actor.principal.name)));
    };
    if !can_claim {
        return Err(DomainError::Forbidden(format!(
            "collector {} is not permitted to claim operations for {unix_user}@{host}",
            actor.principal.name
        )));
    }
    let allowed: Vec<String> = serde_json::from_value(providers_json).unwrap_or_default();
    if allowed.is_empty() || allowed.iter().any(|p| p == "*") {
        return Ok(requested_providers.to_vec());
    }
    if requested_providers.is_empty() {
        return Ok(allowed);
    }
    for req in requested_providers {
        if !allowed.iter().any(|a| a == req) {
            return Err(DomainError::Forbidden(format!("collector {} is not granted provider {req}", actor.principal.name)));
        }
    }
    Ok(requested_providers.to_vec())
}

pub async fn report_operation_receipt(hub: &Hub, actor: &Actor, op_id: Uuid, input: OperationReceiptIn) -> DResult<ProviderOperation> {
    actor.require(OPERATOR_OR_COLLECTOR)?;
    let op = get_operation(hub, actor, op_id).await?;
    if actor.principal.role != contracts::Role::Operator {
        let claimed_by: Option<String> = sqlx::query_scalar("select claimed_by from model_operations where id = $1")
            .bind(op_id)
            .fetch_optional(&hub.db)
            .await?
            .flatten();
        if claimed_by.as_deref() != Some(actor.name().as_str()) {
            return Err(DomainError::Forbidden(format!("collector {} did not claim operation {op_id}", actor.principal.name)));
        }
    }
    let safe_receipt = input.receipt.map(|r| capture::redact_json(&r).0);
    let safe_err = input.error.map(|e| capture::redact_text(&e).0);
    if input.status == OperationStatus::Succeeded {
        apply_hub_side_effects(hub, &op.provider_id, &op.command).await?;
    }
    update_operation_state(&hub.db, op_id, input.status, safe_receipt, safe_err).await?;
    get_operation(hub, actor, op_id).await
}

// ── Panel Settings & Notifications ──────────────────────────────────────────

pub async fn get_settings(hub: &Hub, actor: &Actor) -> DResult<ProviderPanelSettings> {
    actor.require(ANY_READER)?;
    let raw: Option<Value> =
        sqlx::query_scalar("select value from settings where key = 'provider_panel_settings'").fetch_optional(&hub.db).await?;
    Ok(raw.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
}

pub async fn set_settings(hub: &Hub, actor: &Actor, settings: ProviderPanelSettings) -> DResult<ProviderPanelSettings> {
    actor.require(OPERATOR)?;
    if !(5..=3600).contains(&settings.refresh_interval_secs) {
        return Err(invalid("invalid_refresh_interval", "refresh_interval_secs must be between 5 and 3600"));
    }
    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "insert into settings (key, value, updated_at) values ('provider_panel_settings', $1, now())
         on conflict (key) do update set value = excluded.value, updated_at = now()",
    )
    .bind(serde_json::to_value(&settings).map_err(|e| DomainError::Internal(e.to_string()))?)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        "providers.settings.updated",
        "provider_settings",
        "panel",
        json!({ "mask_emails": settings.mask_emails, "tray_slots": settings.tray_slots.len() }),
    )
    .await?;
    tx.commit().await?;
    Ok(settings)
}

/// Translate observed quota exhaustion / cooldown / probe failures into Hub notifications
/// (respecting `mask_emails` and notification toggle settings).
pub async fn sync_quota_notifications(hub: &Hub, provider_id: &str) -> DResult<()> {
    let settings = get_settings(hub, &internal_operator("provider-notify")).await.unwrap_or_default();
    let accounts = load_accounts_with_latest_windows(&hub.db, provider_id).await?;
    let source = format!("providers:{provider_id}");
    let mut items = Vec::new();
    let now = Utc::now();

    for acct in &accounts {
        if !acct.enabled {
            continue;
        }
        let label = if settings.mask_emails {
            acct.masked_email.as_deref().unwrap_or(&acct.display_name)
        } else {
            acct.email.as_deref().unwrap_or(&acct.display_name)
        };
        if settings.notify_limit_hit {
            if acct.cooldown_until.is_some_and(|u| u > now) {
                items.push(crate::notifications::Raise {
                    source: source.clone(),
                    dedupe_key: format!("provider:{provider_id}:{}:cooldown", acct.id),
                    severity: Severity::Warning,
                    title: format!("{provider_id} · {label} rate-limited"),
                    body: acct.status_message.clone().unwrap_or_else(|| "Account is in cooldown until reset.".to_string()),
                    subject_type: Some("model_provider".into()),
                    subject_id: None,
                    url: None,
                });
            }
            for win in &acct.windows {
                if win.observation_state == QuotaObservationState::Measured && win.remaining_percent.is_some_and(|rem| rem <= 5.0) {
                    items.push(crate::notifications::Raise {
                        source: source.clone(),
                        dedupe_key: format!("provider:{provider_id}:{}:{}:low", acct.id, win.window_key),
                        severity: if win.remaining_percent.unwrap_or(0.0) <= 0.5 { Severity::Critical } else { Severity::Warning },
                        title: format!("{provider_id} · {label} {} low ({:.0}% left)", win.label, win.remaining_percent.unwrap_or(0.0)),
                        body: match win.resets_at {
                            Some(r) => format!("Resets at {}", r.format("%Y-%m-%d %H:%M UTC")),
                            None => "Reset time unknown.".into(),
                        },
                        subject_type: Some("model_provider".into()),
                        subject_id: None,
                        url: None,
                    });
                }
            }
        }
    }

    let _ = crate::notifications::sync(hub, &source, items).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_window(key: &str, used: Option<f64>, state: QuotaObservationState) -> QuotaWindow {
        QuotaWindow {
            window_key: key.into(),
            label: "5-hour".into(),
            kind: QuotaWindowKind::Session,
            unit: QuotaUnit::Percent,
            used_percent: used,
            remaining_percent: used.map(|u| 100.0 - u),
            used_amount: None,
            limit_amount: None,
            currency: None,
            resets_at: Some(Utc::now() + Duration::hours(3)),
            window_duration_mins: Some(300),
            cadence: ResetCadence::RollingOnUse,
            scope_model: None,
            status: QuotaStatus::Allowed,
            observation_state: state,
            observed_at: Some(Utc::now()),
            stale: false,
            error_reason: None,
        }
    }

    fn acct(id: &str, enabled: bool, tier: Option<f64>, fp: Option<&str>, win: QuotaWindow) -> ModelAccount {
        ModelAccount {
            id: id.into(),
            provider_id: "test".into(),
            source_account_id: id.into(),
            display_name: id.into(),
            email: Some(format!("{id}@example.com")),
            masked_email: Some(mask_email(&format!("{id}@example.com"))),
            note: None,
            plan: None,
            tier_multiplier: tier,
            status: AccountStatus::Ready,
            enabled,
            priority: 0,
            weight: 1,
            cooldown_until: None,
            status_message: None,
            is_active: false,
            is_preferred: false,
            is_next_pick: false,
            enrollment_state: EnrollmentState::Verified,
            host: None,
            unix_user: None,
            credential_fingerprint: fp.map(String::from),
            last_used_at: None,
            last_observed_at: Some(Utc::now()),
            capabilities: BTreeSet::new(),
            success_count: 0,
            failure_count: 0,
            windows: vec![win],
            reset_grant: None,
            rolling_starter: None,
        }
    }

    #[test]
    fn pool_skips_unobserved_and_deduplicates_fingerprints() {
        let a = acct("a", true, Some(5.0), Some("fp1"), sample_window("five_hour", Some(20.0), QuotaObservationState::Measured));
        let dup = acct("a-dup", true, Some(5.0), Some("fp1"), sample_window("five_hour", Some(20.0), QuotaObservationState::Measured));
        let b = acct("b", true, Some(1.0), Some("fp2"), sample_window("five_hour", Some(80.0), QuotaObservationState::Measured));
        let missing = acct("c", true, Some(5.0), Some("fp3"), sample_window("five_hour", None, QuotaObservationState::NeverObserved));
        let pools = compute_pools("test", &[a, dup, b, missing], &[]);
        assert_eq!(pools.len(), 1);
        let p = &pools[0];
        assert_eq!(p.eligible_accounts, 3, "duplicate fingerprint collapsed");
        assert_eq!(p.included_accounts, 2, "unobserved never coerced to 0");
        // Weighted used = (20*5 + 80*1) / 6 = 180 / 6 = 30%
        assert!((p.weighted_used_percent.unwrap() - 30.0).abs() < 1e-6);
        assert!((p.weighted_remaining_percent.unwrap() - 70.0).abs() < 1e-6);
    }

    #[test]
    fn account_status_respects_spillway_precedence() {
        let now = Utc::now();
        let future = Some(now + Duration::minutes(10));
        assert_eq!(AccountStatus::resolve(false, future, now, true, true), AccountStatus::Disabled);
        assert_eq!(AccountStatus::resolve(true, future, now, true, true), AccountStatus::Limited);
        assert_eq!(AccountStatus::resolve(true, None, now, true, true), AccountStatus::NextPick);
        assert_eq!(AccountStatus::resolve(true, None, now, false, true), AccountStatus::Error);
        assert_eq!(AccountStatus::resolve(true, None, now, false, false), AccountStatus::Ready);
    }
}
