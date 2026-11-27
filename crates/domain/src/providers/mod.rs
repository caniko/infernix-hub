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

use crate::notifications::{self, Raise};
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

/// Remove fixture provider instances and `fixture-host` accounts (with their
/// quota snapshots, samples, and reset grants) when running in live mode.
pub async fn purge_fixture_data(hub: &Hub) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    sqlx::query("delete from model_provider_instances where mode = 'fixture'").execute(&mut *tx).await?;
    sqlx::query(
        "with removed as (
             delete from model_accounts
             where host = 'fixture-host'
               and source_publisher is null
               and publisher_host is null
             returning provider_id, id
         ),
         del_latest as (
             delete from model_quota_latest m
             using removed r
             where m.provider_id = r.provider_id and m.account_id = r.id
         ),
         del_samples as (
             delete from model_quota_samples s
             using removed r
             where s.provider_id = r.provider_id and s.account_id = r.id
         )
         delete from model_reset_grants g
         using removed r
         where g.provider_id = r.provider_id and g.account_id = r.id",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
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
    ingest_batch(hub, actor, provider_id, batch, false).await
}

pub async fn ingest_pushed_observations(hub: &Hub, actor: &Actor, provider_id: &str, batch: ObservationBatch) -> DResult<ObservationAck> {
    ingest_batch(hub, actor, provider_id, batch, true).await
}

async fn ingest_batch(hub: &Hub, actor: &Actor, provider_id: &str, batch: ObservationBatch, pushed: bool) -> DResult<ObservationAck> {
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

    let provider_written = sqlx::query(
        "insert into model_provider_instances
            (id, label, family, mode, verification, router_id, health, status_message, capabilities, evidence_kinds, last_observed_at, updated_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
         on conflict (id) do update set
            label = excluded.label,
            family = excluded.family,
            mode = excluded.mode,
            verification = excluded.verification,
            router_id = coalesce(excluded.router_id, model_provider_instances.router_id),
            health = excluded.health,
            status_message = excluded.status_message,
            capabilities = case when jsonb_array_length(excluded.capabilities) > 0 then excluded.capabilities else model_provider_instances.capabilities end,
            evidence_kinds = case when jsonb_array_length(excluded.evidence_kinds) > 0 then excluded.evidence_kinds else model_provider_instances.evidence_kinds end,
            last_observed_at = greatest(model_provider_instances.last_observed_at, excluded.last_observed_at),
            updated_at = now()
         where model_provider_instances.last_observed_at is null or excluded.last_observed_at >= model_provider_instances.last_observed_at",
    )
    .bind(provider_id)
    .bind(&batch.provider_label)
    .bind(&batch.provider_family)
    .bind(mode.as_str())
    .bind(verification.as_str())
    .bind(&batch.router_id)
    .bind(&health_str)
    .bind(batch.status_message.as_deref().map(sanitize_provider_error))
    .bind(&caps_json)
    .bind(&evs_json)
    .bind(batch.collected_at)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if provider_written == 0 && mode == AdapterMode::Live {
        // A newer fixture snapshot must not keep a live provider labelled as a fixture.
        sqlx::query(
            "update model_provider_instances set mode = $2, verification = $3, updated_at = now() where id = $1 and mode = 'fixture'",
        )
        .bind(provider_id)
        .bind(mode.as_str())
        .bind(verification.as_str())
        .execute(&mut *tx)
        .await?;
    }
    if mode == AdapterMode::Live {
        sqlx::query(
            "with removed as (
                 delete from model_accounts
                 where provider_id = $1
                   and host = 'fixture-host'
                   and source_publisher is null
                   and publisher_host is null
                 returning id
             ),
             del_grants as (
                 delete from model_reset_grants
                 where provider_id = $1 and account_id in (select id from removed)
             )
             delete from model_quota_latest
             where provider_id = $1 and account_id in (select id from removed)",
        )
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;
    }

    let source_publisher = batch.source_publisher.as_deref().filter(|id| !id.trim().is_empty());
    let publisher = (pushed && mode != AdapterMode::Fixture).then_some((batch.source_host.as_str(), batch.source_user.as_str()));
    if let Some((host, user)) = publisher {
        let accepted = sqlx::query(
            "insert into model_provider_publishers (provider_id, source_host, source_user, source_publisher, last_observed_at)
             values ($1, $2, $3, $4, $5)
             on conflict (provider_id, source_host, source_user, source_publisher) do update
             set last_observed_at = greatest(model_provider_publishers.last_observed_at, excluded.last_observed_at)
             where excluded.last_observed_at >= model_provider_publishers.last_observed_at",
        )
        .bind(provider_id)
        .bind(host)
        .bind(user)
        .bind(source_publisher.unwrap_or_default())
        .bind(batch.collected_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if accepted == 0 {
            return Ok(ObservationAck {
                provider_id: provider_id.into(),
                accounts_upserted: 0,
                samples_inserted: 0,
                grants_upserted: 0,
                recorded_at: Utc::now(),
            });
        }
    }

    let mut accounts_upserted = 0usize;
    let mut samples_inserted = 0usize;
    let mut grants_upserted = 0usize;

    // Some collectors mirror proxy rows with the same ID. They can refresh
    // evidence without taking over the existing producer's removal rights.
    for acct in &batch.accounts {
        let masked = acct.masked_email.clone().or_else(|| acct.email.as_deref().map(mask_email));
        let enroll_str = serde_json::to_value(acct.enrollment_state)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unobserved".into());
        let acct_caps = serde_json::to_value(&acct.capabilities).unwrap_or_else(|_| json!([]));
        let starter_json = acct.rolling_starter.as_ref().and_then(|s| serde_json::to_value(s).ok());
        let error = acct.last_error.clone().map(AccountError::sanitized);
        let error = error.as_ref();
        let status_message = acct
            .status_message
            .as_deref()
            .map(sanitize_provider_error)
            .filter(|s| !s.is_empty())
            .or_else(|| error.map(AccountError::summary));
        // A legacy writer can report a failure, but omission is not evidence
        // that a previous typed incident has recovered.
        let reports_error_state = error.is_some()
            || (acct.last_error_observed && acct.enrollment_state != EnrollmentState::Error && acct.status != AccountStatus::Error);
        let row = sqlx::query(
            "insert into model_accounts
                (id, provider_id, source_account_id, display_name, email, masked_email, note, plan,
                 tier_multiplier, enabled, priority, weight, cooldown_until, status_message,
                 is_active, is_preferred, is_next_pick, enrollment_state, host, unix_user,
                 credential_fingerprint, last_used_at, last_observed_at, capabilities,
                 success_count, failure_count, rolling_starter,
                 error_kind, error_source, error_http_status, error_code, error_message,
                 error_action_hint, error_action_url, error_at, error_consecutive_count, error_backoff_until,
                  observation_batch_at, publisher_host, publisher_user, source_publisher, updated_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8,
                     $9, $10, $11, $12, $13, $14,
                     $15, $16, $17, $18, $19, $20,
                     $21, $22, $23, $24,
                      $25, $26, $27, $28, $29, $30, $31, $32, $33, $34, $35, $36, $37, $38, $39, $40, $42, now())
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
                status_message = case when $41 or model_accounts.error_kind is null then excluded.status_message
                                      else model_accounts.status_message end,
                is_active = excluded.is_active,
                is_preferred = excluded.is_preferred,
                is_next_pick = excluded.is_next_pick,
                enrollment_state = excluded.enrollment_state,
                host = coalesce(excluded.host, model_accounts.host),
                unix_user = coalesce(excluded.unix_user, model_accounts.unix_user),
                credential_fingerprint = coalesce(excluded.credential_fingerprint, model_accounts.credential_fingerprint),
                last_used_at = coalesce(excluded.last_used_at, model_accounts.last_used_at),
                last_observed_at = greatest(model_accounts.last_observed_at, excluded.last_observed_at),
                capabilities = excluded.capabilities,
                success_count = greatest(model_accounts.success_count, excluded.success_count),
                failure_count = greatest(model_accounts.failure_count, excluded.failure_count),
                 rolling_starter = coalesce(excluded.rolling_starter, model_accounts.rolling_starter),
                 error_kind = case when $41 then excluded.error_kind else model_accounts.error_kind end,
                 error_source = case when $41 then excluded.error_source else model_accounts.error_source end,
                 error_http_status = case when $41 then excluded.error_http_status else model_accounts.error_http_status end,
                 error_code = case when $41 then excluded.error_code else model_accounts.error_code end,
                 error_message = case when $41 then excluded.error_message else model_accounts.error_message end,
                 error_action_hint = case when $41 then excluded.error_action_hint else model_accounts.error_action_hint end,
                 error_action_url = case when $41 then excluded.error_action_url else model_accounts.error_action_url end,
                 error_at = case when $41 then excluded.error_at else model_accounts.error_at end,
                 error_consecutive_count = case when $41 then excluded.error_consecutive_count else model_accounts.error_consecutive_count end,
                 error_backoff_until = case when $41 then excluded.error_backoff_until else model_accounts.error_backoff_until end,
                 observation_batch_at = excluded.observation_batch_at,
                 publisher_host = coalesce(model_accounts.publisher_host, excluded.publisher_host),
                  publisher_user = coalesce(model_accounts.publisher_user, excluded.publisher_user),
                  source_publisher = coalesce(model_accounts.source_publisher, excluded.source_publisher),
                 updated_at = now()
             where (model_accounts.last_observed_at is null or excluded.last_observed_at >= model_accounts.last_observed_at)
               and (model_accounts.observation_batch_at is null or excluded.observation_batch_at >= model_accounts.observation_batch_at)
               and (model_accounts.publisher_host is null or
                    (model_accounts.publisher_host, model_accounts.publisher_user) = (excluded.publisher_host, excluded.publisher_user))",
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
        .bind(&status_message)
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
        .bind(error.map(|e| e.kind.as_str()))
        .bind(error.map(|e| e.source.as_str()))
        .bind(error.and_then(|e| e.http_status).map(i32::from))
        .bind(error.and_then(|e| e.provider_code.as_deref()))
        .bind(error.map(|e| e.message.as_str()))
        .bind(error.and_then(|e| e.action_hint.as_deref()))
        .bind(error.and_then(|e| e.action_url.as_deref()))
        .bind(error.map(|e| e.occurred_at))
        .bind(error.map_or(0, |e| i32::try_from(e.consecutive_count).unwrap_or(i32::MAX)))
        .bind(error.and_then(|e| e.backoff_until))
        .bind(batch.collected_at)
        .bind(publisher.map(|p| p.0))
        .bind(publisher.map(|p| p.1))
        .bind(reports_error_state)
        .bind(source_publisher)
        .execute(&mut *tx)
        .await?;
        if row.rows_affected() == 0 {
            continue;
        }
        accounts_upserted += 1;

        if !acct.windows.is_empty() {
            let mut max_incoming_observed_at: Option<DateTime<Utc>> = None;
            let mut incoming_keys: Vec<String> = Vec::with_capacity(acct.windows.len());
            let mut incoming_scopes: Vec<String> = Vec::with_capacity(acct.windows.len());
            for win in &acct.windows {
                let obs_at = win.observed_at.unwrap_or(batch.collected_at);
                if max_incoming_observed_at.is_none_or(|prev| obs_at > prev) {
                    max_incoming_observed_at = Some(obs_at);
                }
                insert_quota_sample(&mut tx, provider_id, &acct.id, win, obs_at).await?;
                samples_inserted += 1;
                incoming_keys.push(win.window_key.clone());
                incoming_scopes.push(win.scope_model.as_deref().unwrap_or("").to_string());
            }
            if let Some(max_obs) = max_incoming_observed_at {
                sqlx::query(
                    "delete from model_quota_latest m
                     where m.provider_id = $1
                       and m.account_id = $2
                       and m.observed_at <= $3
                       and not exists (
                           select 1
                           from unnest($4::text[], $5::text[]) as incoming(window_key, scope_model_key)
                           where incoming.window_key = m.window_key
                             and incoming.scope_model_key = m.scope_model_key
                       )",
                )
                .bind(provider_id)
                .bind(&acct.id)
                .bind(max_obs)
                .bind(&incoming_keys)
                .bind(&incoming_scopes)
                .execute(&mut *tx)
                .await?;
            }
        }
        if let Some(grant) = &acct.reset_grant {
            upsert_reset_grant(&mut tx, provider_id, &acct.id, grant).await?;
            grants_upserted += 1;
        }
    }

    if let (Some((host, user)), Some(source_publisher)) = (publisher.filter(|_| batch.account_inventory_complete), source_publisher) {
        let ids: Vec<_> = batch.accounts.iter().map(|a| a.id.as_str()).collect();
        sqlx::query(
            "with removed as (
                 delete from model_accounts
                 where provider_id = $1
                   and id <> all($6)
                   and (
                       (publisher_host = $2 and publisher_user = $3
                        and source_publisher = $4 and observation_batch_at <= $5)
                       or ($4 = 'fleetix-agent-proxy'
                           and host = $2 and unix_user = $3
                           and (source_publisher is null or observation_batch_at is null)
                           and (source_account_id like 'proxy:%' or source_account_id like 'local-%'))
                   )
                 returning id
             ),
             del_grants as (
                 delete from model_reset_grants
                 where provider_id = $1 and account_id in (select id from removed)
             )
             delete from model_quota_latest
             where provider_id = $1 and account_id in (select id from removed)",
        )
        .bind(provider_id)
        .bind(host)
        .bind(user)
        .bind(source_publisher)
        .bind(batch.collected_at)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
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
        // Only Antigravity uses the proxy pool as its quota source. Claude and
        // OpenCode also use direct-login quota, even when their proxy pool is empty.
        let routed_account_ids = (provider_id == "antigravity"
            && routing.router_id.as_deref() == Some("fleetix-proxy")
            && source_publisher == Some("fleetix-agent-proxy")
            && batch.account_inventory_complete)
            .then(|| batch.accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>());
        let routed_accounts_collected_at = routed_account_ids.as_ref().map(|_| batch.collected_at);
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
                (provider_id, router_id, revision, runtime, policy, clients, lockouts, recent_logs, rolling_starter,
                 routed_account_ids, routed_accounts_collected_at, updated_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now())
             on conflict (provider_id) do update set
                router_id = excluded.router_id,
                revision = greatest(provider_routing_state.revision, excluded.revision),
                runtime = excluded.runtime,
                policy = excluded.policy,
                clients = excluded.clients,
                lockouts = excluded.lockouts,
                recent_logs = excluded.recent_logs,
                rolling_starter = excluded.rolling_starter,
                routed_account_ids = case
                    when excluded.routed_accounts_collected_at >= coalesce(provider_routing_state.routed_accounts_collected_at, '-infinity')
                    then excluded.routed_account_ids else provider_routing_state.routed_account_ids end,
                routed_accounts_collected_at = greatest(excluded.routed_accounts_collected_at, provider_routing_state.routed_accounts_collected_at),
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
        .bind(routed_account_ids)
        .bind(routed_accounts_collected_at)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Sync independently so an unavailable source cannot block the other.
    if let Err(error) = sync_quota_notifications(hub, provider_id).await {
        tracing::warn!(%provider_id, %error, "provider quota notification sync failed");
    }
    if let Err(error) = sync_provider_health_notifications(hub, provider_id).await {
        tracing::warn!(%provider_id, %error, "provider health notification sync failed");
    }

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
) -> DResult<i64> {
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
    let scope_model = win.scope_model.as_deref().filter(|s| !s.is_empty());
    let scope_model_key = scope_model.unwrap_or("");
    let sanitized_error = win.error_reason.as_deref().map(sanitize_provider_error);

    let sample_id: i64 = sqlx::query_scalar(
        "insert into model_quota_samples
            (provider_id, account_id, window_key, label, kind, unit, used_percent, remaining_percent,
             used_amount, limit_amount, currency, resets_at, window_duration_mins, cadence, scope_model,
             status, observation_state, observed_at, stale, error_reason)
         values ($1, $2, $3, $4, $5, $6, $7, $8,
                 $9, $10, $11, $12, $13, $14, $15,
                 $16, $17, $18, $19, $20)
         returning id",
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
    .bind(scope_model)
    .bind(&status)
    .bind(&obs_state)
    .bind(observed_at)
    .bind(win.stale)
    .bind(&sanitized_error)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "insert into model_quota_latest
            (sample_id, provider_id, account_id, window_key, scope_model_key, label, kind, unit,
             used_percent, remaining_percent, used_amount, limit_amount, currency, resets_at,
             window_duration_mins, cadence, scope_model, status, observation_state, observed_at,
             stale, error_reason)
         values ($1, $2, $3, $4, $5, $6, $7, $8,
                 $9, $10, $11, $12, $13, $14,
                 $15, $16, $17, $18, $19, $20,
                 $21, $22)
         on conflict (provider_id, account_id, window_key, scope_model_key) do update set
            sample_id = excluded.sample_id,
            label = excluded.label,
            kind = excluded.kind,
            unit = excluded.unit,
            used_percent = excluded.used_percent,
            remaining_percent = excluded.remaining_percent,
            used_amount = excluded.used_amount,
            limit_amount = excluded.limit_amount,
            currency = excluded.currency,
            resets_at = excluded.resets_at,
            window_duration_mins = excluded.window_duration_mins,
            cadence = excluded.cadence,
            scope_model = excluded.scope_model,
            status = excluded.status,
            observation_state = excluded.observation_state,
            observed_at = excluded.observed_at,
            recorded_at = now(),
            stale = excluded.stale,
            error_reason = excluded.error_reason
         where excluded.observed_at > model_quota_latest.observed_at
            or (excluded.observed_at = model_quota_latest.observed_at and excluded.sample_id >= model_quota_latest.sample_id)",
    )
    .bind(sample_id)
    .bind(provider_id)
    .bind(account_id)
    .bind(&win.window_key)
    .bind(scope_model_key)
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
    .bind(scope_model)
    .bind(&status)
    .bind(&obs_state)
    .bind(observed_at)
    .bind(win.stale)
    .bind(&sanitized_error)
    .execute(tx)
    .await?;

    Ok(sample_id)
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
    let desc = load_descriptor(hub, provider_id).await?;
    load_accounts_with_latest_windows(&hub.db, provider_id, None, None, desc.mode == AdapterMode::Live).await
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
    const HISTORY_LIMIT: usize = 4000;

    actor.require(ANY_READER)?;
    let as_of = Utc::now();
    let desc = load_descriptor(hub, provider_id).await?;
    let routed = if query.account_id.is_none() { routed_account_ids(&hub.db, provider_id).await? } else { None };
    let accounts =
        load_accounts_with_latest_windows(&hub.db, provider_id, Some(as_of), routed.as_deref(), desc.mode == AdapterMode::Live).await?;
    let range_hours = query.range_hours.unwrap_or(48).clamp(1, 24 * 35);
    let lookback_hours = query.lookback_hours.unwrap_or(6).clamp(1, 168);
    let horizon_hours = query.horizon_hours.unwrap_or(12).clamp(1, 168);
    let since = as_of - Duration::hours(range_hours as i64);
    let fetch_since = as_of - Duration::hours(range_hours.max(lookback_hours) as i64);

    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(s) from (
            select distinct on (account_id, window_key, coalesce(scope_model, ''), observed_at)
                   id, provider_id, account_id, window_key, nullif(scope_model, '') as scope_model, label, kind, unit,
                   used_percent, remaining_percent, used_amount, limit_amount,
                   resets_at, window_duration_mins, cadence, status, observation_state,
                   observed_at, recorded_at, stale, error_reason
            from model_quota_samples
            where provider_id = $1
              and observed_at between $2 and $3
              and ($4::text is null or account_id = $4)
              and ($5::text is null or window_key = $5)
              and ($6::text[] is null or account_id = any($6))
            order by account_id, window_key, coalesce(model_quota_samples.scope_model, ''), observed_at, id desc
         ) s
         order by s.observed_at desc, s.id desc
         limit $7",
    )
    .bind(provider_id)
    .bind(fetch_since)
    .bind(as_of)
    .bind(&query.account_id)
    .bind(&query.window_key)
    .bind(routed.as_deref())
    .bind((HISTORY_LIMIT + 1) as i64)
    .fetch_all(&hub.db)
    .await?;
    let mut samples: Vec<QuotaSample> = crate::decode(rows)?;
    let history_truncated = samples.get(HISTORY_LIMIT).is_some_and(|sample| sample.observed_at >= since);
    samples.truncate(HISTORY_LIMIT);
    samples.reverse();

    let forecasts = compute_forecasts(&samples, &accounts, lookback_hours, horizon_hours, as_of);
    // The forecast lookback can be wider than the history requested by the chart.
    samples.drain(..samples.partition_point(|sample| sample.observed_at < since));
    let past_windows = compute_past_windows(&samples);
    let pools = compute_pools(provider_id, &accounts, &forecasts);

    Ok(ProviderQuotaReport { provider: desc, accounts, pools, samples, history_truncated, past_windows, forecasts, as_of })
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
    let routed_account_ids = routed_account_ids(&hub.db, &descriptor.id).await?.map(|ids| ids.into_iter().collect::<BTreeSet<_>>());
    let accounts = load_accounts_with_latest_windows(&hub.db, &descriptor.id, None, None, descriptor.mode == AdapterMode::Live).await?;
    let pool_accounts = accounts.iter().filter(|a| routed_account_ids.as_ref().is_none_or(|ids| ids.contains(&a.id)));
    let account_count = pool_accounts.clone().count();
    let enabled_accounts = pool_accounts.clone().filter(|a| a.enabled).count();
    let active_account_id = pool_accounts.clone().find(|a| a.is_active).map(|a| a.id.clone());
    let preferred_account_id = pool_accounts.clone().find(|a| a.is_preferred).map(|a| a.id.clone());
    let next_pick_account_id = pool_accounts.clone().find(|a| a.is_next_pick).map(|a| a.id.clone());
    let pools = compute_pools(&descriptor.id, pool_accounts, &[]);
    Ok(ProviderSummary {
        descriptor,
        account_count,
        enabled_accounts,
        active_account_id,
        preferred_account_id,
        next_pick_account_id,
        pools,
        accounts,
        routed_account_ids,
    })
}

async fn routed_account_ids(db: &sqlx::PgPool, provider_id: &str) -> DResult<Option<Vec<String>>> {
    Ok(sqlx::query_scalar("select routed_account_ids from provider_routing_state where provider_id = $1")
        .bind(provider_id)
        .fetch_optional(db)
        .await?
        .flatten())
}

async fn load_accounts_with_latest_windows(
    db: &sqlx::PgPool,
    provider_id: &str,
    as_of: Option<DateTime<Utc>>,
    account_ids: Option<&[String]>,
    exclude_fixture_host: bool,
) -> DResult<Vec<ModelAccount>> {
    let acct_rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(a) from (
            select id, provider_id, source_account_id, display_name, email, masked_email, note, plan,
                   tier_multiplier, enabled, priority, weight, cooldown_until, status_message,
                   is_active, is_preferred, is_next_pick, enrollment_state, host, unix_user,
                   credential_fingerprint, last_used_at, last_observed_at, capabilities,
                   success_count, failure_count, rolling_starter,
                   case when error_kind is not null then jsonb_build_object(
                       'kind', error_kind, 'source', error_source, 'http_status', error_http_status,
                       'provider_code', error_code, 'message', error_message, 'action_hint', error_action_hint,
                       'action_url', error_action_url, 'occurred_at', error_at, 'consecutive_count', error_consecutive_count,
                       'backoff_until', error_backoff_until
                   ) end as last_error
            from model_accounts
            where provider_id = $1
              and ($2::text[] is null or id = any($2))
              and (not $3 or not (host = 'fixture-host' and source_publisher is null and publisher_host is null))
            order by priority desc, id asc
         ) a",
    )
    .bind(provider_id)
    .bind(account_ids)
    .bind(exclude_fixture_host)
    .fetch_all(db)
    .await?;

    let win_rows: Vec<Value> = if let Some(at) = as_of {
        sqlx::query_scalar(
            "select to_jsonb(w) from (
                select distinct on (account_id, window_key, coalesce(scope_model, ''))
                    account_id, window_key, label, kind, unit, used_percent, remaining_percent,
                    used_amount, limit_amount, currency, resets_at, window_duration_mins,
                    cadence, nullif(scope_model, '') as scope_model, status, observation_state, observed_at, stale, error_reason
                from model_quota_samples
                where provider_id = $1 and observed_at <= $2
                  and ($3::text[] is null or account_id = any($3))
                order by account_id, window_key, coalesce(model_quota_samples.scope_model, ''), observed_at desc, id desc
             ) w",
        )
        .bind(provider_id)
        .bind(at)
        .bind(account_ids)
        .fetch_all(db)
        .await?
    } else {
        sqlx::query_scalar(
            "select to_jsonb(w) from (
                select account_id, window_key, label, kind, unit, used_percent, remaining_percent,
                       used_amount, limit_amount, currency, resets_at, window_duration_mins,
                       cadence, nullif(scope_model, '') as scope_model, status, observation_state, observed_at, stale, error_reason
                from model_quota_latest
                where provider_id = $1
                  and ($2::text[] is null or account_id = any($2))
                order by account_id, window_key, scope_model_key
             ) w",
        )
        .bind(provider_id)
        .bind(account_ids)
        .fetch_all(db)
        .await?
    };

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

    let now = as_of.unwrap_or_else(Utc::now);
    let mut out = Vec::with_capacity(acct_rows.len());
    for row in acct_rows {
        let mut acct: ModelAccount = crate::decode_one(row)?;
        let mut windows = windows_by_acct.remove(&acct.id).unwrap_or_default();
        // Ignore a superseded synthetic `quota` probe-failure marker once a newer good observation arrives.
        let latest_good = windows
            .iter()
            .filter(|w| !matches!(w.observation_state, QuotaObservationState::ProbeFailed))
            .filter_map(|w| w.observed_at)
            .max();
        if let Some(good_at) = latest_good {
            windows.retain(|w| {
                !(w.window_key == "quota"
                    && matches!(w.observation_state, QuotaObservationState::ProbeFailed)
                    && w.observed_at.is_none_or(|fail_at| fail_at <= good_at))
            });
        }
        dedupe_superseded_windows(&mut windows);
        acct.windows = windows;
        acct.reset_grant = grants_by_acct.remove(&acct.id);
        let has_err = acct.enrollment_state == EnrollmentState::Error
            || (acct.last_error.is_none()
                && acct.windows.iter().any(|w| matches!(w.observation_state, QuotaObservationState::ProbeFailed)));
        acct.status =
            AccountStatus::resolve_with_error(acct.enabled, acct.cooldown_until, now, acct.is_next_pick, has_err, acct.last_error.as_ref());
        out.push(acct);
    }
    Ok(out)
}

/// Drop any unscoped (`scope_model == None`) window when a model-scoped window
/// with the same `window_key` exists at the same or newer `observed_at`.
fn dedupe_superseded_windows(windows: &mut Vec<QuotaWindow>) {
    let mut max_scoped_at: BTreeMap<String, Option<DateTime<Utc>>> = BTreeMap::new();
    for w in windows.iter() {
        if w.scope_model.as_deref().is_some_and(|s| !s.is_empty()) {
            max_scoped_at
                .entry(w.window_key.clone())
                .and_modify(|prev| {
                    if w.observed_at > *prev {
                        *prev = w.observed_at;
                    }
                })
                .or_insert(w.observed_at);
        }
    }
    windows.retain(|w| {
        if w.scope_model.as_deref().is_none_or(str::is_empty)
            && let Some(scoped_at) = max_scoped_at.get(&w.window_key)
        {
            return match (scoped_at, w.observed_at) {
                (Some(s), Some(n)) => *s < n,
                (Some(_), None) | (None, None) => false,
                (None, Some(_)) => true,
            };
        }
        true
    });
}

/// Combine compatible windows across distinct eligible enabled accounts.
/// Never averages missing/unobserved accounts as zero and never treats routing
/// weight as quota capacity (uses plan `tier_multiplier` when available).
pub fn compute_pools<'a>(
    provider_id: &str,
    accounts: impl IntoIterator<Item = &'a ModelAccount>,
    forecasts: &[QuotaForecast],
) -> Vec<PoolWindowSummary> {
    // Deduplicate logical entitlements by credential_fingerprint when present.
    let mut seen_fp: BTreeSet<String> = BTreeSet::new();
    let mut distinct_eligible: Vec<&ModelAccount> = Vec::new();
    for acct in accounts.into_iter().filter(|a| a.enabled) {
        if let Some(fp) = &acct.credential_fingerprint
            && !seen_fp.insert(fp.clone())
        {
            continue;
        }
        distinct_eligible.push(acct);
    }

    let mut by_window = BTreeMap::<_, Vec<_>>::new();
    for acct in &distinct_eligible {
        for win in &acct.windows {
            if win.unit != QuotaUnit::Percent {
                continue;
            }
            by_window.entry((win.window_key.as_str(), win.scope_model.as_deref())).or_default().push((acct, win));
        }
    }

    let mut pools = Vec::new();
    for ((window_key, scope_model), entries) in by_window {
        let Some((_, first)) = entries.first() else {
            continue;
        };
        let mut weighted_used = 0.0;
        let mut total_capacity = 0.0;
        let mut included = 0usize;
        let mut any_tier_weight = false;
        let mut next_reset: Option<DateTime<Utc>> = None;
        let mut window_seconds = None;

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
            let seconds = win.window_duration_mins.filter(|&mins| mins > 0).map(|mins| u64::from(mins) * 60).or(match win.kind {
                QuotaWindowKind::Daily => Some(86_400),
                QuotaWindowKind::Weekly => Some(604_800),
                _ if win.window_key == "5h" => Some(18_000),
                _ => None,
            });
            if included == 0 {
                window_seconds = seconds;
            } else if window_seconds != seconds {
                window_seconds = None;
            }
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

        let forecast = forecasts
            .iter()
            .find(|f| f.window_key == window_key && f.scope_model.as_deref() == scope_model && f.account_id.is_none())
            .cloned();
        pools.push(PoolWindowSummary {
            provider_id: provider_id.to_string(),
            window_key: window_key.to_owned(),
            scope_model: scope_model.map(str::to_owned),
            label: first.label.clone(),
            kind: first.kind,
            eligible_accounts: distinct_eligible.len(),
            included_accounts: included,
            weighted_used_percent: used_pct,
            weighted_remaining_percent: rem_pct,
            capacity_basis: if any_tier_weight { "plan_tier_multiplier".into() } else { "equal_entitlements".into() },
            next_reset_at: next_reset,
            window_seconds,
            forecast,
            is_estimate: true,
        });
    }
    pools
}

/// Partition ordered quota samples into past completed windows by detecting
/// reset boundaries (either `resets_at` advancing or a sharp drop in `used_percent`).
pub fn compute_past_windows(samples: &[QuotaSample]) -> Vec<PastWindowSummary> {
    let mut grouped = BTreeMap::<_, Vec<_>>::new();
    for s in samples {
        if s.used_percent.is_some() {
            grouped.entry(quota_series_key(s)).or_default().push(s);
        }
    }

    let mut out = Vec::new();
    for series in grouped.into_values() {
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
                if let Some(summary) = summarize_segment(&segment, curr.observed_at) {
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

fn quota_series_key(sample: &QuotaSample) -> (&str, &str, Option<&str>) {
    (&sample.account_id, &sample.window_key, sample.scope_model.as_deref())
}

fn summarize_segment(segment: &[&QuotaSample], next_seen: DateTime<Utc>) -> Option<PastWindowSummary> {
    let first = *segment.first()?;
    let last = *segment.last()?;
    let peak = segment.iter().filter_map(|s| s.used_percent).fold(None::<f64>, |acc, v| Some(acc.map_or(v, |a| a.max(v))));
    let exhausted_at = segment.iter().find(|s| s.used_percent.is_some_and(|u| u >= 99.5)).map(|s| s.observed_at);
    let early_replaced = last.resets_at.is_some_and(|r| next_seen + Duration::minutes(10) < r);
    Some(PastWindowSummary {
        account_id: first.account_id.clone(),
        window_key: first.window_key.clone(),
        scope_model: first.scope_model.clone(),
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
    let mut by_acct_win = BTreeMap::<_, Vec<_>>::new();
    for s in samples {
        if (cutoff..=now).contains(&s.observed_at) && s.used_percent.is_some() {
            by_acct_win.entry(quota_series_key(s)).or_default().push(s);
        }
    }

    let mut forecasts = Vec::new();
    type PoolRateSample = (f64, f64, Option<DateTime<Utc>>);
    let mut pool_rates = BTreeMap::<_, Vec<PoolRateSample>>::new();

    for ((account_id, window_key, scope_model), series) in by_acct_win {
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
            .and_then(|a| a.windows.iter().find(|w| w.window_key == window_key && w.scope_model.as_deref() == scope_model))
            .and_then(|w| w.resets_at)
            .or(last.resets_at);

        pool_rates.entry((window_key, scope_model)).or_default().push((current_used, drain_per_hour, resets_at));
        forecasts.push(build_forecast(
            Some(account_id.to_owned()),
            window_key,
            scope_model,
            current_used,
            drain_per_hour,
            resets_at,
            lookback_hours,
            horizon_hours,
            now,
        ));
    }

    // Aggregate pool forecasts across accounts, never across model scopes.
    for ((window_key, scope_model), entries) in pool_rates {
        let n = entries.len() as f64;
        if n <= 0.0 {
            continue;
        }
        let avg_used = entries.iter().map(|(u, _, _)| *u).sum::<f64>() / n;
        let avg_drain = entries.iter().map(|(_, r, _)| *r).sum::<f64>() / n;
        let next_reset = entries.iter().filter_map(|(_, _, r)| *r).min();
        forecasts.push(build_forecast(None, window_key, scope_model, avg_used, avg_drain, next_reset, lookback_hours, horizon_hours, now));
    }

    forecasts
}

#[allow(clippy::too_many_arguments)]
fn build_forecast(
    account_id: Option<String>,
    window_key: &str,
    scope_model: Option<&str>,
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
        window_key: window_key.to_owned(),
        scope_model: scope_model.map(str::to_owned),
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
                let mut tx = hub.db.begin().await?;
                apply_hub_side_effects(&mut tx, op_id, provider_id, &input.command, Some(&safe_receipt)).await?;
                update_operation_state(&mut *tx, op_id, OperationStatus::Succeeded, Some(safe_receipt), None).await?;
                tx.commit().await?;
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
async fn apply_hub_side_effects(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op_id: Uuid,
    provider_id: &str,
    command: &ProviderCommand,
    receipt: Option<&Value>,
) -> DResult<()> {
    match command {
        ProviderCommand::Account(AccountCommand::SetEnabled { account_id, enabled }) => {
            sqlx::query("update model_accounts set enabled = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(enabled)
                .execute(&mut **tx)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SetPriority { account_id, priority }) => {
            sqlx::query("update model_accounts set priority = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(priority)
                .execute(&mut **tx)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SetWeight { account_id, weight }) => {
            sqlx::query("update model_accounts set weight = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(*weight as i32)
                .execute(&mut **tx)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SetNote { account_id, note }) => {
            sqlx::query("update model_accounts set note = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .bind(note)
                .execute(&mut **tx)
                .await?;
        }
        ProviderCommand::Account(AccountCommand::SelectLocalAccount { account_id, .. })
        | ProviderCommand::Account(AccountCommand::SelectPersistentAccount { account_id, .. }) => {
            sqlx::query("update model_accounts set is_active = (id = $2), updated_at = now() where provider_id = $1")
                .bind(provider_id)
                .bind(account_id)
                .execute(&mut **tx)
                .await?;
        }
        ProviderCommand::Routing(RoutingCommand::SetPoolOverride { account_id }) => {
            sqlx::query(
                "update model_accounts set is_preferred = ($2::text is not null and id = $2), updated_at = now() where provider_id = $1",
            )
            .bind(provider_id)
            .bind(account_id)
            .execute(&mut **tx)
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
            .execute(&mut **tx)
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
            .execute(&mut **tx)
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
            .execute(&mut **tx)
            .await?;
        }
        ProviderCommand::Probe(ProbeCommand::ConfigureCanarySchedule { schedule }) => {
            sqlx::query(
                "insert into settings (key, value, updated_at) values ('provider_canary_schedule', $1, now())
                 on conflict (key) do update set value = excluded.value, updated_at = now()",
            )
            .bind(serde_json::to_value(schedule).unwrap_or_else(|_| json!({})))
            .execute(&mut **tx)
            .await?;
        }
        ProviderCommand::Probe(ProbeCommand::RunCanary { model_id, effort, .. }) => {
            let ran_at = receipt
                .and_then(|r| r.get("ran_at").and_then(Value::as_str))
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let run = CanaryRun {
                id: op_id,
                provider_id: provider_id.to_string(),
                model_id: model_id.clone(),
                effort: effort.clone(),
                prompts_run: receipt.and_then(|r| r.get("prompts_run").and_then(Value::as_u64)).unwrap_or(0) as u32,
                prompts_passed: receipt.and_then(|r| r.get("prompts_passed").and_then(Value::as_u64)).unwrap_or(0) as u32,
                mean_thinking_tokens: receipt.and_then(|r| r.get("mean_thinking_tokens").and_then(Value::as_f64)).unwrap_or(0.0),
                mean_ttft_ms: receipt.and_then(|r| r.get("mean_ttft_ms").and_then(Value::as_f64)),
                ran_at,
                error: receipt.and_then(|r| r.get("error").and_then(Value::as_str)).map(str::to_owned),
            };
            sqlx::query(
                "insert into model_probe_runs (id, kind, provider_id, payload, ran_at)
                 values ($1, 'canary', $2, $3, $4)
                 on conflict (id) do nothing",
            )
            .bind(op_id)
            .bind(provider_id)
            .bind(serde_json::to_value(&run).unwrap_or_else(|_| json!({})))
            .bind(ran_at)
            .execute(&mut **tx)
            .await?;
        }
        ProviderCommand::Probe(ProbeCommand::RunThinkingTest {
            model_id, effort, compare_model_id, compare_effort, sample_count, ..
        }) => {
            let ran_at = receipt
                .and_then(|r| r.get("ran_at").and_then(Value::as_str))
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            let run = ThinkingTestRun {
                id: op_id,
                baseline_model: model_id.clone(),
                baseline_effort: effort.clone(),
                compare_model: compare_model_id.clone().unwrap_or_else(|| model_id.clone()),
                compare_effort: compare_effort.clone(),
                sampled_turns: receipt.and_then(|r| r.get("sampled_turns").and_then(Value::as_u64)).unwrap_or(u64::from(*sample_count))
                    as u32,
                judge_model: receipt.and_then(|r| r.get("judge_model").and_then(Value::as_str)).map(str::to_owned),
                equivalence_rate: receipt.and_then(|r| r.get("equivalence_rate").and_then(Value::as_f64)),
                measured_cost_delta_pct: receipt.and_then(|r| r.get("measured_cost_delta_pct").and_then(Value::as_f64)),
                ran_at,
                status: receipt.and_then(|r| r.get("status").and_then(Value::as_str)).unwrap_or("completed").to_string(),
            };
            sqlx::query(
                "insert into model_probe_runs (id, kind, provider_id, payload, ran_at)
                 values ($1, 'thinking_test', $2, $3, $4)
                 on conflict (id) do nothing",
            )
            .bind(op_id)
            .bind(provider_id)
            .bind(serde_json::to_value(&run).unwrap_or_else(|_| json!({})))
            .bind(ran_at)
            .execute(&mut **tx)
            .await?;
        }
        ProviderCommand::KeyGroup(KeyGroupCommand::DeleteGroup { group_id }) => {
            sqlx::query("delete from model_key_groups where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(group_id)
                .execute(&mut **tx)
                .await?;
        }
        ProviderCommand::KeyGroup(KeyGroupCommand::SetGroupEnabled { group_id, enabled }) => {
            sqlx::query("update model_key_groups set enabled = $3, updated_at = now() where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(group_id)
                .bind(enabled)
                .execute(&mut **tx)
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
            .execute(&mut **tx)
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
            .execute(&mut **tx)
            .await?;
        }
        ProviderCommand::Credential(CredentialCommand::RemoveLogin { account_id }) => {
            sqlx::query("delete from model_quota_latest where provider_id = $1 and account_id = $2")
                .bind(provider_id)
                .bind(account_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("delete from model_reset_grants where provider_id = $1 and account_id = $2")
                .bind(provider_id)
                .bind(account_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("delete from model_accounts where provider_id = $1 and id = $2")
                .bind(provider_id)
                .bind(account_id)
                .execute(&mut **tx)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn update_operation_state<'e, E>(
    exec: E,
    op_id: Uuid,
    status: OperationStatus,
    receipt: Option<Value>,
    error: Option<String>,
) -> DResult<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let status_str = serde_json::to_value(status).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| "failed".into());
    sqlx::query("update model_operations set status = $2, receipt = coalesce($3, receipt), error = $4, updated_at = now() where id = $1")
        .bind(op_id)
        .bind(&status_str)
        .bind(&receipt)
        .bind(&error)
        .execute(exec)
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
    let mut tx = hub.db.begin().await?;
    let locked: Option<(String, String, Value, Option<String>)> =
        sqlx::query_as("select provider_id, status, command, claimed_by from model_operations where id = $1 for update")
            .bind(op_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((provider_id, current_status, command_json, claimed_by)) = locked else {
        return Err(DomainError::NotFound("provider operation"));
    };
    if actor.principal.role != contracts::Role::Operator && claimed_by.as_deref() != Some(actor.name().as_str()) {
        return Err(DomainError::Forbidden(format!("collector {} did not claim operation {op_id}", actor.principal.name)));
    }
    // Terminal operations are idempotent: duplicate receipt retries return the existing result without reapplying side effects.
    if matches!(current_status.as_str(), "succeeded" | "failed" | "rejected" | "cancelled") {
        tx.commit().await?;
        return get_operation(hub, actor, op_id).await;
    }
    let command: ProviderCommand = crate::decode_one(command_json)?;
    let safe_receipt = input.receipt.map(|r| capture::redact_json(&r).0);
    let safe_err = input.error.map(|e| capture::redact_text(&e).0);
    if input.status == OperationStatus::Succeeded {
        apply_hub_side_effects(&mut tx, op_id, &provider_id, &command, safe_receipt.as_ref()).await?;
    }
    update_operation_state(&mut *tx, op_id, input.status, safe_receipt, safe_err).await?;
    tx.commit().await?;
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
    if let Err(error) = reconcile_provider_health_notifications(hub).await {
        tracing::warn!(%error, "provider health notification reconciliation after settings update failed");
    }
    Ok(settings)
}

/// Refresh alert sources even if their collectors have stopped pushing.
pub async fn reconcile_quota_notifications(hub: &Hub) -> DResult<()> {
    let provider_ids: Vec<String> = sqlx::query_scalar("select id from model_provider_instances").fetch_all(&hub.db).await?;
    for provider_id in provider_ids {
        if let Err(error) = sync_quota_notifications(hub, &provider_id).await {
            tracing::warn!(%provider_id, %error, "provider quota notification reconciliation failed");
        }
    }
    Ok(())
}

pub async fn sync_quota_notifications(hub: &Hub, provider_id: &str) -> DResult<()> {
    let settings = get_settings(hub, &internal_operator("provider-notify")).await?;
    let exclude_fixture = load_descriptor(hub, provider_id).await.map(|d| d.mode == AdapterMode::Live).unwrap_or(true);
    let accounts = load_accounts_with_latest_windows(&hub.db, provider_id, None, None, exclude_fixture).await?;
    let source = format!("providers:{provider_id}");
    notifications::sync(hub, &source, quota_raises(provider_id, &settings, &accounts, Utc::now())).await?;
    Ok(())
}

/// Refresh health and staleness even when a provider has stopped reporting.
pub async fn reconcile_provider_health_notifications(hub: &Hub) -> DResult<()> {
    let settings = get_settings(hub, &internal_operator("provider-notify")).await?;
    for provider in load_descriptors(hub).await? {
        if let Err(error) = sync_provider_health(hub, &provider, &settings).await {
            tracing::warn!(provider_id = %provider.id, %error, "provider health notification reconciliation failed");
        }
    }
    Ok(())
}

pub async fn sync_provider_health_notifications(hub: &Hub, provider_id: &str) -> DResult<()> {
    let settings = get_settings(hub, &internal_operator("provider-notify")).await?;
    let provider = load_descriptor(hub, provider_id).await?;
    sync_provider_health(hub, &provider, &settings).await
}

async fn sync_provider_health(hub: &Hub, provider: &ProviderDescriptor, settings: &ProviderPanelSettings) -> DResult<()> {
    let accounts = load_accounts_with_latest_windows(&hub.db, &provider.id, None, None, provider.mode == AdapterMode::Live).await?;
    let publishers = sqlx::query_as(
        "select source_host, source_user, max(last_observed_at) from model_provider_publishers
         where provider_id = $1 group by source_host, source_user order by source_host, source_user",
    )
    .bind(&provider.id)
    .fetch_all(&hub.db)
    .await?;
    let items = {
        let offline = hub.expected_offline_hosts.read().unwrap_or_else(|error| error.into_inner());
        let now = Utc::now();
        let mut items = provider_health_raises(provider, settings, &accounts, now);
        items.extend(stale_publisher_raises(provider, publishers, &offline, now));
        items
    };
    notifications::sync(hub, &format!("providers-health:{}", provider.id), items).await?;
    Ok(())
}

/// Keep conventional keys stable and suffix noncanonical keys with a digest so
/// long, mixed-case, and punctuation-heavy IDs do not collide when normalized.
pub fn provider_notification_key(provider_id: &str, tail: &str) -> String {
    let raw = format!("provider:{provider_id}:{tail}");
    let mut key: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-') { c.to_ascii_lowercase() } else { '-' })
        .collect();
    if key != raw || key.len() > 128 || provider_id.contains(':') {
        let digest = hash::sha256_hex(format!("{provider_id}\0{tail}").as_bytes());
        key.truncate(115);
        key.push('-');
        key.push_str(&digest[..12]);
    }
    key
}

fn account_error_key(provider_id: &str, account_id: &str) -> String {
    // These tails already belong to the provider and its poller.
    let suffix = if matches!(account_id, "health" | "poller") { "error:account" } else { "error" };
    provider_notification_key(provider_id, &format!("{account_id}:{suffix}"))
}

fn account_notification_label(acct: &ModelAccount, mask_emails: bool) -> String {
    if mask_emails {
        acct.masked_email
            .clone()
            .or_else(|| acct.email.as_deref().map(mask_email))
            .unwrap_or_else(|| if acct.display_name.contains('@') { mask_email(&acct.display_name) } else { acct.display_name.clone() })
    } else {
        acct.email.as_ref().unwrap_or(&acct.display_name).clone()
    }
}

fn account_error_alert(acct: &ModelAccount) -> Option<(Severity, &'static str)> {
    use AccountErrorKind::*;
    use Severity::{Critical, Warning};
    if acct.last_error.as_ref().is_some_and(|e| !e.is_notification_worthy()) {
        return None;
    }
    Some(match acct.last_error.as_ref().map(|e| e.kind) {
        Some(VerificationRequired) => (Critical, "requires verification"),
        Some(AccountSuspended) => (Critical, "suspended"),
        Some(AuthExpired) => (Critical, "authentication expired"),
        Some(BillingRequired) => (Critical, "billing/credits required"),
        Some(PermissionDenied) => (Warning, "permission denied"),
        Some(ProbeFailed) => (Warning, "quota probe failing"),
        Some(UpstreamUnavailable) => (Warning, "upstream errors"),
        Some(Other) => (Warning, "error"),
        None if acct.enrollment_state == EnrollmentState::Error => (Critical, "enrollment error"),
        None if acct.windows.iter().any(|w| w.observation_state == QuotaObservationState::ProbeFailed) => (Warning, "quota probe failing"),
        _ => return None,
    })
}

fn provider_health_raises(
    provider: &ProviderDescriptor,
    settings: &ProviderPanelSettings,
    accounts: &[ModelAccount],
    now: DateTime<Utc>,
) -> Vec<Raise> {
    let source = format!("providers-health:{}", provider.id);
    let label = sanitize_provider_error(&provider.label);
    let raise = |dedupe_key, severity, title, body| Raise {
        source: source.clone(),
        dedupe_key,
        severity,
        title,
        body,
        subject_type: Some("model_provider".into()),
        subject_id: Some(provider.id.clone()),
        url: None,
    };
    let enabled: Vec<_> = accounts.iter().filter(|a| a.enabled).collect();
    let account_label = |acct| {
        let mut label = sanitize_provider_error(&account_notification_label(acct, settings.mask_emails));
        label.truncate(label.floor_char_boundary(240));
        label
    };
    let mut items = Vec::new();
    for acct in &enabled {
        let Some((severity, detail)) = account_error_alert(acct) else { continue };
        let body = if let Some(error) = acct.last_error.clone().map(AccountError::sanitized) {
            let mut lines = vec![format!("Error kind: {}", error.kind.as_str()), format!("Source: {}", error.source.as_str())];
            if let Some(code) = error.provider_code {
                lines.push(format!("Provider code: {code}"));
            }
            lines.extend(error.action_hint);
            lines.join("\n")
        } else {
            format!("Account {detail}.")
        };
        let account_label = account_label(acct);
        let body = if let Some(email) = acct.email.as_deref().filter(|email| settings.mask_emails && !email.is_empty()) {
            body.replace(email, &mask_email(email))
        } else {
            body
        };
        items.push(raise(account_error_key(&provider.id, &acct.id), severity, format!("{label}: {account_label} {detail}"), body));
    }

    let unavailable = |a: &ModelAccount| {
        a.enrollment_state == EnrollmentState::Error
            || a.last_error.as_ref().is_some_and(|e| e.blocks_routing(now))
            || a.cooldown_until.is_some_and(|until| until > now)
    };
    if !enabled.is_empty() && enabled.iter().all(|a| unavailable(a)) {
        const DISPLAY_LIMIT: usize = 8;
        let mut reasons: Vec<_> = enabled
            .iter()
            .take(DISPLAY_LIMIT)
            .map(|a| {
                let reason = match (&a.last_error, a.cooldown_until) {
                    (Some(e), _) if e.blocks_routing(now) => e.kind.label().to_owned(),
                    (_, Some(until)) if until > now => format!("cooldown until {}", until.format("%Y-%m-%d %H:%M UTC")),
                    _ => "Account error".into(),
                };
                format!("{}: {reason}", account_label(a))
            })
            .collect();
        if enabled.len() > DISPLAY_LIMIT {
            reasons.push(format!("… and {} more", enabled.len() - DISPLAY_LIMIT));
        }
        items.push(raise(
            provider_notification_key(&provider.id, "pool:unavailable"),
            Severity::Critical,
            format!("{label}: all accounts unavailable"),
            reasons.join("\n"),
        ));
    }
    if items.is_empty()
        && let Some(severity) = match provider.health {
            ProviderHealth::Offline => Some(Severity::Critical),
            ProviderHealth::Degraded => Some(Severity::Warning),
            _ => None,
        }
    {
        items.push(raise(
            provider_notification_key(&provider.id, "health:error"),
            severity,
            format!("{label}: provider health {}", if severity == Severity::Critical { "error" } else { "degraded" }),
            provider
                .status_message
                .as_deref()
                .map(sanitize_provider_error)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "No additional provider details were reported.".into()),
        ));
    }
    items
}

fn stale_publisher_raises<'a>(
    provider: &'a ProviderDescriptor,
    publishers: Vec<(String, String, DateTime<Utc>)>,
    expected_offline_hosts: &'a [String],
    now: DateTime<Utc>,
) -> impl Iterator<Item = Raise> + 'a {
    publishers.into_iter().filter_map(move |(host, user, last)| {
        if provider.mode == AdapterMode::Fixture || now - last <= Duration::minutes(10) || expected_offline_hosts.contains(&host) {
            return None;
        }
        let publisher = hash::sha256_hex(format!("{host}\0{user}").as_bytes());
        Some(Raise {
            source: format!("providers-health:{}", provider.id),
            dedupe_key: provider_notification_key(&provider.id, &format!("collector:{}:stale", &publisher[..24])),
            severity: Severity::Warning,
            title: format!("{}: proxy/collector stopped reporting", sanitize_provider_error(&provider.label)),
            body: format!(
                "Publisher: {}\nLast observation: {}",
                sanitize_provider_error(&format!("{user}@{host}")),
                last.format("%Y-%m-%d %H:%M UTC")
            ),
            subject_type: Some("model_provider".into()),
            subject_id: Some(provider.id.clone()),
            url: None,
        })
    })
}

const QUOTA_CRITICAL_PERCENT: f64 = 0.5;

fn antigravity_bucket_scope(provider_id: &str, win: &QuotaWindow) -> Option<&'static str> {
    if provider_id != "antigravity" {
        return None;
    }
    // Summary and models-fallback observations use different labels and session keys for the same routing bucket.
    ["gemini-argon", "gemini", "3p"]
        .into_iter()
        .find(|scope| win.window_key.strip_prefix(scope).is_some_and(|suffix| matches!(suffix, "-5h" | "-quota" | "-weekly")))
}

fn has_quota_fallback(provider_id: &str, accounts: &[ModelAccount], acct: &ModelAccount, win: &QuotaWindow, now: DateTime<Utc>) -> bool {
    let bucket = antigravity_bucket_scope(provider_id, win);
    let scope = bucket.or(win.scope_model.as_deref());
    let active = |w: &QuotaWindow| w.resets_at.is_none_or(|reset| reset > now);
    accounts.iter().any(|other| {
        if !other.enabled
            || other.weight == 0
            || other.id == acct.id
            || other.provider_id != acct.provider_id
            || other.credential_fingerprint.as_ref().is_some_and(|fp| Some(fp) == acct.credential_fingerprint.as_ref())
            || other.cooldown_until.is_some_and(|until| until > now)
            || other.enrollment_state == EnrollmentState::Error
            || other.last_error.as_ref().is_some_and(|error| error.blocks_routing(now))
        {
            return false;
        }
        let mut measured_capacity = false;
        for w in other.windows.iter().filter(|w| active(w)) {
            let other_bucket = antigravity_bucket_scope(provider_id, w);
            let other_scope = other_bucket.or(w.scope_model.as_deref());
            let exhausted = w.status == QuotaStatus::Rejected
                || w.used_percent.is_some_and(|p| p >= 100.0)
                || w.remaining_percent.is_some_and(|p| p <= 0.0);
            if exhausted && w.kind != QuotaWindowKind::Spend && (other_scope.is_none() || other_scope == scope) {
                return false;
            }
            let matching_window = w.window_key == win.window_key
                || (bucket.is_some() && bucket == other_bucket && w.window_key.ends_with("-weekly") == win.window_key.ends_with("-weekly"));
            measured_capacity |= other_scope == scope
                && matching_window
                && !exhausted
                && !w.stale
                && w.observation_state == QuotaObservationState::Measured
                && w.remaining_percent.is_some_and(|p| p > 0.0);
        }
        measured_capacity
    })
}

fn quota_raises(provider_id: &str, settings: &ProviderPanelSettings, accounts: &[ModelAccount], now: DateTime<Utc>) -> Vec<Raise> {
    let source = format!("providers:{provider_id}");
    let mut items = Vec::new();

    for acct in accounts {
        if !acct.enabled {
            continue;
        }
        let label = account_notification_label(acct, settings.mask_emails);
        if settings.notify_limit_hit {
            if acct.cooldown_until.is_some_and(|u| u > now) {
                items.push(Raise {
                    source: source.clone(),
                    dedupe_key: provider_notification_key(provider_id, &format!("{}:cooldown", acct.id)),
                    severity: Severity::Warning,
                    title: format!("{provider_id} · {label} rate-limited"),
                    body: acct.status_message.clone().unwrap_or_else(|| "Account is in cooldown until reset.".to_string()),
                    subject_type: Some("model_provider".into()),
                    subject_id: None,
                    url: None,
                });
            }
            // Keep the existing alert key when model scopes share a window key.
            let mut lowest = BTreeMap::new();
            for win in &acct.windows {
                if let Some(remaining) = win.remaining_percent
                    && remaining <= 5.0
                    && win.observation_state == QuotaObservationState::Measured
                    && win.resets_at.is_none_or(|reset| reset > now)
                {
                    let critical = remaining <= QUOTA_CRITICAL_PERCENT && !has_quota_fallback(provider_id, accounts, acct, win, now);
                    let entry = lowest.entry(&win.window_key).or_insert((win, remaining, critical));
                    if (critical && !entry.2) || (critical == entry.2 && remaining < entry.1) {
                        *entry = (win, remaining, critical);
                    }
                }
            }
            for (win, remaining, critical) in lowest.into_values() {
                items.push(Raise {
                    source: source.clone(),
                    dedupe_key: provider_notification_key(provider_id, &format!("{}:{}:low", acct.id, win.window_key)),
                    severity: if critical { Severity::Critical } else { Severity::Warning },
                    title: format!("{provider_id} · {label} {} low ({remaining:.0}% left)", win.label),
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

    items
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
            last_error: None,
            last_error_observed: false,
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

    fn account_error(kind: AccountErrorKind, consecutive_count: u32) -> AccountError {
        AccountError {
            kind,
            source: AccountErrorSource::Inference,
            http_status: Some(403),
            provider_code: Some("TEST_CODE".into()),
            message: "Try again without Bearer private-token or https://example.com?secret=private-query".into(),
            action_hint: Some("Open the provider dashboard".into()),
            action_url: Some("https://example.com".into()),
            occurred_at: Utc::now(),
            consecutive_count,
            backoff_until: None,
        }
    }

    fn health_provider() -> ProviderDescriptor {
        ProviderDescriptor {
            id: "test".into(),
            label: "Test Provider".into(),
            family: "test".into(),
            mode: AdapterMode::Live,
            verification: Verification::Built,
            evidence_kinds: Default::default(),
            capabilities: Default::default(),
            router_id: None,
            health: ProviderHealth::Healthy,
            status_message: None,
            last_observed_at: None,
        }
    }

    #[test]
    fn health_alerts_cover_each_account_error_and_threshold() {
        use AccountErrorKind::*;
        let provider = health_provider();
        let now = Utc::now();
        let mut a = acct("primary", true, None, None, sample_window("quota", None, QuotaObservationState::NeverObserved));
        let settings = ProviderPanelSettings { mask_emails: true, ..Default::default() };
        for (kind, threshold, severity, suffix) in [
            (VerificationRequired, 0, Severity::Critical, "requires verification"),
            (AccountSuspended, 0, Severity::Critical, "suspended"),
            (AuthExpired, 0, Severity::Critical, "authentication expired"),
            (BillingRequired, 0, Severity::Critical, "billing/credits required"),
            (PermissionDenied, 0, Severity::Warning, "permission denied"),
            (ProbeFailed, 2, Severity::Warning, "quota probe failing"),
            (UpstreamUnavailable, 3, Severity::Warning, "upstream errors"),
            (Other, 2, Severity::Warning, "error"),
        ] {
            for count in 0..=3 {
                a.last_error = Some(account_error(kind, count));
                let items = provider_health_raises(&provider, &settings, std::slice::from_ref(&a), now);
                let found = items.iter().find(|r| r.dedupe_key == "provider:test:primary:error");
                assert_eq!(found.is_some(), count >= threshold, "{kind:?}/{count}");
                if let Some(r) = found {
                    assert_eq!(r.source, "providers-health:test");
                    assert_eq!(r.severity, severity);
                    assert_eq!(r.title, format!("Test Provider: {} {suffix}", a.masked_email.as_deref().unwrap()));
                    assert_eq!(r.subject_type.as_deref(), Some("model_provider"));
                    assert_eq!(r.subject_id.as_deref(), Some("test"));
                    assert_eq!(r.url, None);
                    for fragment in ["TEST_CODE", "inference", "Open the provider dashboard"] {
                        assert!(r.body.contains(fragment), "{fragment}: {}", r.body);
                    }
                    assert!(!r.body.contains("private-"));
                }
            }
        }
        a.last_error = Some(account_error(QuotaExhausted, 5));
        a.enrollment_state = EnrollmentState::Error;
        a.windows[0].observation_state = QuotaObservationState::ProbeFailed;
        assert!(
            provider_health_raises(&provider, &settings, std::slice::from_ref(&a), now)
                .iter()
                .all(|r| r.dedupe_key == "provider:test:pool:unavailable")
        );
        a.last_error = None;
        let legacy = |a: &ModelAccount| provider_health_raises(&provider, &settings, std::slice::from_ref(a), now);
        assert!(legacy(&a)[0].title.ends_with("enrollment error"));
        assert_eq!(legacy(&a)[0].severity, Severity::Critical);
        a.enrollment_state = EnrollmentState::Verified;
        assert!(legacy(&a)[0].title.ends_with("quota probe failing"));
        assert_eq!(legacy(&a)[0].severity, Severity::Warning);
        a.enabled = false;
        assert!(legacy(&a).is_empty());
    }

    #[test]
    fn health_alerts_bound_keys_and_pool_details_without_affecting_quota() {
        let provider = health_provider();
        let now = Utc::now();
        let mut failed =
            acct(&format!("{}🌱A", "a".repeat(150)), true, None, None, sample_window("quota", None, QuotaObservationState::NeverObserved));
        failed.last_error = Some(account_error(AccountErrorKind::VerificationRequired, 1));
        let mut limited = acct("secondary", true, None, None, sample_window("five_hour", Some(100.0), QuotaObservationState::Measured));
        limited.status = AccountStatus::Limited;
        limited.cooldown_until = Some(now + Duration::minutes(5));
        let settings = ProviderPanelSettings { mask_emails: true, notify_limit_hit: true, ..Default::default() };
        let mut accounts = vec![failed, limited];
        let raises = |a: &[ModelAccount], now| provider_health_raises(&provider, &settings, a, now);
        let items = raises(&accounts, now);
        let key = &items[0].dedupe_key;
        assert!(key.len() <= 128 && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b":._-".contains(&b)));
        assert_eq!(*key, raises(&accounts, now)[0].dedupe_key);
        assert_ne!(*key, provider_notification_key("test", &format!("{}🌱B:error", "a".repeat(150))));
        assert_ne!(account_error_key("test", "poller"), provider_notification_key("test", "poller:error"));
        let pool = &items[1];
        assert_eq!(pool.dedupe_key, "provider:test:pool:unavailable");
        assert_eq!(pool.severity, Severity::Critical);
        assert!(pool.body.contains("Verification required"));
        assert!(pool.body.contains("cooldown until"));
        assert!(pool.body.contains(accounts[1].masked_email.as_deref().unwrap()));
        assert!(!pool.body.contains("secondary@example.com"));
        let quota = quota_raises("test", &settings, &accounts, now);
        assert_eq!(quota.len(), 2);
        assert!(quota.iter().all(|r| r.source == "providers:test" && !items.iter().any(|h| h.dedupe_key == r.dedupe_key)));
        accounts[0].last_error = None;
        assert!(raises(&accounts, now).is_empty());
        assert_eq!(quota_raises("test", &settings, &accounts, now).len(), 2);
        accounts[0].enabled = false;
        assert!(raises(&accounts, now + Duration::minutes(5)).is_empty());
        let many_unavailable: Vec<_> = (0..32)
            .map(|i| {
                let mut a = accounts[1].clone();
                a.masked_email = Some(format!("{i}{}@example.net", "á".repeat(500)));
                a
            })
            .collect();
        let body = &raises(&many_unavailable, now)[0].body;
        assert!(body.len() < 2_500);
        assert!(body.contains("and 24 more"));
        let unmasked = ProviderPanelSettings { mask_emails: false, ..settings };
        assert!(provider_health_raises(&provider, &unmasked, &accounts, now)[0].body.contains("secondary@example.com"));
    }

    #[test]
    fn provider_fallback_and_stale_collector_have_independent_lifecycles() {
        let mut provider = health_provider();
        let now = Utc::now();
        let settings = ProviderPanelSettings::default();
        let stale = now - Duration::minutes(11);
        let publishers = vec![
            ("nomad".into(), "can".into(), stale),
            ("atlas".into(), "can".into(), now - Duration::minutes(10)),
            ("atlas".into(), "other".into(), stale),
        ];
        let alerts: Vec<_> = stale_publisher_raises(&provider, publishers.clone(), &["nomad".into()], now).collect();
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].body.contains("other@atlas"));
        assert_eq!(alerts[0].severity, Severity::Warning);
        assert_eq!(stale_publisher_raises(&provider, vec![], &[], now).count(), 0);
        provider.mode = AdapterMode::Fixture;
        assert_eq!(stale_publisher_raises(&provider, publishers, &[], now).count(), 0);
        provider.mode = AdapterMode::Live;
        provider.status_message = Some("No credentials configured".into());
        for (health, severity) in [(ProviderHealth::Degraded, Severity::Warning), (ProviderHealth::Offline, Severity::Critical)] {
            provider.health = health;
            let items = provider_health_raises(&provider, &settings, &[], now);
            assert_eq!(items[0].dedupe_key, "provider:test:health:error");
            assert_eq!(items[0].body, "No credentials configured");
            assert_eq!(items[0].severity, severity);
        }
        let mut a = acct("primary", true, None, None, sample_window("quota", None, QuotaObservationState::NeverObserved));
        a.last_error = Some(account_error(AccountErrorKind::BillingRequired, 1));
        assert!(provider_health_raises(&provider, &settings, &[a], now).iter().all(|r| r.dedupe_key != "provider:test:health:error"));
    }

    #[test]
    fn quota_alert_stays_warning_with_an_eligible_measured_pool_fallback() {
        let now = Utc::now();
        let settings = ProviderPanelSettings { notify_limit_hit: true, ..Default::default() };
        let mut empty = sample_window("gemini-argon-5h", Some(100.0), QuotaObservationState::Measured);
        empty.scope_model = Some("Gemini Argon".into());
        let healthy = QuotaWindow {
            window_key: "gemini-argon-quota".into(),
            scope_model: Some("Gemini 4 Argon".into()),
            used_percent: Some(5.0),
            remaining_percent: Some(95.0),
            ..empty.clone()
        };
        let primary = acct("google-primary", true, None, None, empty);
        let mut fallback = acct("google-agam", true, None, None, healthy);
        let severity = |fallback: &ModelAccount| {
            quota_raises("antigravity", &settings, &[primary.clone(), fallback.clone()], now)
                .into_iter()
                .find(|r| r.dedupe_key.ends_with("google-primary:gemini-argon-5h:low"))
                .unwrap()
                .severity
        };
        assert_eq!(severity(&fallback), Severity::Warning);
        fallback.enabled = false;
        assert_eq!(severity(&fallback), Severity::Critical);
        fallback.enabled = true;
        fallback.cooldown_until = Some(now + Duration::hours(1));
        assert_eq!(severity(&fallback), Severity::Critical);
        fallback.cooldown_until = None;
        fallback.windows[0].observation_state = QuotaObservationState::StaleLastGood;
        assert_eq!(severity(&fallback), Severity::Critical);
        fallback.windows[0].observation_state = QuotaObservationState::Measured;
        fallback.windows[0].window_key = "gemini-quota".into();
        assert_eq!(severity(&fallback), Severity::Critical);
    }

    #[test]
    fn quota_alert_is_critical_when_the_pool_is_exhausted() {
        let now = Utc::now();
        let settings = ProviderPanelSettings { notify_limit_hit: true, ..Default::default() };
        let empty = sample_window("gemini-5h", Some(100.0), QuotaObservationState::Measured);
        let accounts = [acct("primary", true, None, None, empty.clone()), acct("backup", true, None, None, empty)];
        let alerts = quota_raises("antigravity", &settings, &accounts, now);
        assert_eq!(alerts.len(), 2);
        assert!(alerts.iter().all(|r| r.severity == Severity::Critical));
    }

    #[test]
    fn quota_alerts_clear_at_reset_without_clearing_the_future_weekly_window() {
        let reset: DateTime<Utc> = "2026-11-10T03:10:00Z".parse().unwrap();
        let mut session = sample_window("five_hour", Some(100.0), QuotaObservationState::Measured);
        session.scope_model = Some("pro".into());
        session.resets_at = Some(reset);
        let flash = QuotaWindow { scope_model: Some("flash".into()), remaining_percent: Some(4.0), ..session.clone() };
        let mut weekly = sample_window("weekly", Some(98.0), QuotaObservationState::Measured);
        weekly.kind = QuotaWindowKind::Weekly;
        weekly.resets_at = Some(reset + Duration::days(5));
        let mut account = acct("google-primary", true, None, None, session);
        account.windows.push(flash);
        account.windows.push(weekly);
        let settings = ProviderPanelSettings { notify_limit_hit: true, ..Default::default() };
        let raises = |now| quota_raises("antigravity", &settings, std::slice::from_ref(&account), now);
        let items = raises(reset - Duration::seconds(1));
        assert_eq!(items.len(), 2);
        assert_eq!(items.iter().find(|item| item.dedupe_key.ends_with(":five_hour:low")).unwrap().severity, Severity::Critical);
        for now in [reset, reset + Duration::seconds(1)] {
            let items = raises(now);
            assert_eq!(items.len(), 1);
            assert!(items[0].dedupe_key.ends_with(":weekly:low"));
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
        assert_eq!(p.window_seconds, Some(18_000));
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

    #[test]
    fn dedupe_superseded_windows_drops_unscoped_when_scoped_is_same_or_newer() {
        let now = Utc::now();
        let earlier = now - Duration::minutes(5);
        let later = now + Duration::minutes(5);

        let mut unscoped_old = sample_window("3p-5h", Some(40.0), QuotaObservationState::Measured);
        unscoped_old.scope_model = None;
        unscoped_old.observed_at = Some(earlier);

        let mut scoped_newer = sample_window("3p-5h", Some(25.0), QuotaObservationState::Measured);
        scoped_newer.scope_model = Some("claude".into());
        scoped_newer.observed_at = Some(now);

        let mut unscoped_equal = sample_window("gemini-5h", Some(50.0), QuotaObservationState::Measured);
        unscoped_equal.scope_model = None;
        unscoped_equal.observed_at = Some(now);

        let mut scoped_equal = sample_window("gemini-5h", Some(30.0), QuotaObservationState::Measured);
        scoped_equal.scope_model = Some("pro".into());
        scoped_equal.observed_at = Some(now);

        let mut unscoped_newer = sample_window("weekly", Some(10.0), QuotaObservationState::Measured);
        unscoped_newer.scope_model = None;
        unscoped_newer.observed_at = Some(later);

        let mut scoped_older = sample_window("weekly", Some(20.0), QuotaObservationState::Measured);
        scoped_older.scope_model = Some("pro".into());
        scoped_older.observed_at = Some(now);

        let mut unscoped_only = sample_window("daily", Some(15.0), QuotaObservationState::Measured);
        unscoped_only.scope_model = None;
        unscoped_only.observed_at = Some(now);

        let mut windows = vec![unscoped_old, scoped_newer, unscoped_equal, scoped_equal, unscoped_newer, scoped_older, unscoped_only];
        dedupe_superseded_windows(&mut windows);

        let keys: Vec<(&str, Option<&str>)> = windows.iter().map(|w| (w.window_key.as_str(), w.scope_model.as_deref())).collect();
        assert_eq!(
            keys,
            vec![("3p-5h", Some("claude")), ("gemini-5h", Some("pro")), ("weekly", None), ("weekly", Some("pro")), ("daily", None),]
        );
    }
}
