//! Capture policy, retention/budget enforcement, secret redaction,
//! diagnostic bundle previews, portable JSONL archives, and ClickHouse sink state.

use chrono::Utc;
use contracts::providers::{
    CaptureHealth, CapturePolicy, ClearDataIn, ClearDataOut, ClearTarget, ClickHouseConfig, ClickHouseStatus, DiagnosticBundlePreview,
    DiagnosticBundlePreviewIn, PortableArchiveExport, PortableArchiveRequest,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR, audit, hash, invalid};

const SENSITIVE_KEYS: &[&str] = &[
    "authorization",
    "x-api-key",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "token",
    "secret",
    "password",
    "cookie",
    "set-cookie",
];

/// Redact likely secret values in free-form strings before persisting logs or
/// building diagnostic/portable exports.
pub fn redact_text(input: &str) -> (String, bool) {
    let mut out = input.to_string();
    let mut redacted = false;
    for prefix in ["Bearer ", "sk-ant-", "sk-proj-", "sk-", "AIza", "ihub_", "ghp_", "gho_"] {
        let mut search_from = 0usize;
        while search_from < out.len() {
            let Some(rel_pos) = out[search_from..].find(prefix) else {
                break;
            };
            let pos = search_from + rel_pos;
            let start = pos + prefix.len();
            let rest = &out[start..];
            let end_rel = rest.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '}' | ']' | ';')).unwrap_or(rest.len());
            if end_rel == 0 {
                search_from = start;
                continue;
            }
            if rest.starts_with("<redacted>") {
                search_from = start + "<redacted>".len();
                continue;
            }
            out.replace_range(start..start + end_rel, "<redacted>");
            redacted = true;
            search_from = start + "<redacted>".len();
        }
    }
    (out, redacted)
}

/// Recursively redact sensitive JSON fields and token-shaped strings.
pub fn redact_json(value: &Value) -> (Value, bool) {
    match value {
        Value::String(s) => {
            let (clean, hit) = redact_text(s);
            (Value::String(clean), hit)
        }
        Value::Array(items) => {
            let mut hit = false;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let (v, h) = redact_json(item);
                hit |= h;
                out.push(v);
            }
            (Value::Array(out), hit)
        }
        Value::Object(map) => {
            let mut hit = false;
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                let lower = k.to_ascii_lowercase();
                if SENSITIVE_KEYS.iter().any(|s| lower == *s || lower.ends_with("_secret") || lower.ends_with("_token")) {
                    out.insert(k.clone(), Value::String("<redacted>".to_string()));
                    hit = true;
                } else {
                    let (clean, h) = redact_json(v);
                    hit |= h;
                    out.insert(k.clone(), clean);
                }
            }
            (Value::Object(out), hit)
        }
        other => (other.clone(), false),
    }
}

pub async fn get_capture_policy(hub: &Hub, actor: &Actor) -> DResult<CapturePolicy> {
    actor.require(crate::ANY_READER)?;
    let raw: Option<Value> =
        sqlx::query_scalar("select value from settings where key = 'provider_capture_policy'").fetch_optional(&hub.db).await?;
    let mut policy: CapturePolicy = raw.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();

    let (stored_bodies, stored_bytes): (i64, i64) =
        sqlx::query_as("select count(*)::bigint, coalesce(sum(size_bytes), 0)::bigint from activity_captures").fetch_one(&hub.db).await?;
    let (dropped, rejected): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where capture_state = 'dropped')::bigint,
                count(*) filter (where capture_state = 'rejected')::bigint
         from activity_exchanges",
    )
    .fetch_one(&hub.db)
    .await?;
    policy.health = CaptureHealth {
        stored_bodies: stored_bodies.max(0) as u64,
        stored_bytes: stored_bytes.max(0) as u64,
        gaps: policy.health.gaps,
        dropped: dropped.max(0) as u64,
        rejected: rejected.max(0) as u64,
        over_budget_evictions: policy.health.over_budget_evictions,
    };
    Ok(policy)
}

pub async fn set_capture_policy(hub: &Hub, actor: &Actor, mut input: CapturePolicy) -> DResult<CapturePolicy> {
    actor.require(OPERATOR)?;
    if !(1..=365).contains(&input.detail_retention_days) {
        return Err(invalid("invalid_retention", "detail_retention_days must be between 1 and 365"));
    }
    if !(1..=90).contains(&input.body_retention_days) {
        return Err(invalid("invalid_retention", "body_retention_days must be between 1 and 90"));
    }
    if !(1..=1024).contains(&input.body_budget_mib) {
        return Err(invalid("invalid_budget", "body_budget_mib must be between 1 and 1024"));
    }
    // Preserve live health counters rather than trusting caller-submitted health.
    let current = get_capture_policy(hub, actor).await?;
    input.health = current.health;

    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "insert into settings (key, value, updated_at) values ('provider_capture_policy', $1, now())
         on conflict (key) do update set value = excluded.value, updated_at = now()",
    )
    .bind(serde_json::to_value(&input).map_err(|e| DomainError::Internal(e.to_string()))?)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        "providers.capture_policy.updated",
        "provider_settings",
        "capture_policy",
        json!({
            "capture_bodies_enabled": input.capture_bodies_enabled,
            "detail_retention_days": input.detail_retention_days,
            "body_retention_days": input.body_retention_days,
            "body_budget_mib": input.body_budget_mib
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(input)
}

/// Evict expired details/bodies and enforce the capture byte budget by oldest session/exchange.
pub async fn enforce_retention(hub: &Hub) -> DResult<u64> {
    let raw: Option<Value> =
        sqlx::query_scalar("select value from settings where key = 'provider_capture_policy'").fetch_optional(&hub.db).await?;
    let policy: CapturePolicy = raw.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();

    let mut evicted = 0u64;
    let body_res = sqlx::query(
        "delete from activity_captures
         where (expires_at is not null and expires_at <= now())
            or created_at < now() - make_interval(days => $1)",
    )
    .bind(policy.body_retention_days as i32)
    .execute(&hub.db)
    .await?;
    evicted += body_res.rows_affected();

    let detail_res = sqlx::query(
        "delete from activity_exchanges
         where occurred_at < now() - make_interval(days => $1)",
    )
    .bind(policy.detail_retention_days as i32)
    .execute(&hub.db)
    .await?;
    evicted += detail_res.rows_affected();

    let budget_bytes = (policy.body_budget_mib as i64) * 1024 * 1024;
    let total_bytes: i64 =
        sqlx::query_scalar("select coalesce(sum(size_bytes), 0)::bigint from activity_captures").fetch_one(&hub.db).await?;
    if total_bytes > budget_bytes {
        let excess = total_bytes - budget_bytes;
        let over_res = sqlx::query(
            "with ordered as (
                select id, sum(size_bytes) over (order by created_at asc, id asc) as running_bytes
                from activity_captures
             )
             delete from activity_captures
             where id in (select id from ordered where running_bytes <= $1 + 1048576)",
        )
        .bind(excess)
        .execute(&hub.db)
        .await?;
        evicted += over_res.rows_affected();
    }
    Ok(evicted)
}

/// Independently clear quota history, captured bodies, retained details, or all activity.
pub async fn clear_data(hub: &Hub, actor: &Actor, input: ClearDataIn) -> DResult<ClearDataOut> {
    actor.require(OPERATOR)?;
    let mut tx = hub.db.begin().await?;
    let rows_deleted = match input.target {
        ClearTarget::QuotaHistory => sqlx::query("delete from model_quota_samples").execute(&mut *tx).await?.rows_affected(),
        ClearTarget::CapturedBodies => {
            let n = sqlx::query("delete from activity_captures").execute(&mut *tx).await?.rows_affected();
            sqlx::query("update activity_exchanges set capture_state = 'evicted' where capture_state in ('transformed', 'raw_wire')")
                .execute(&mut *tx)
                .await?;
            n
        }
        ClearTarget::ActivityDetails => {
            // Preserves `activity_totals` and `activity_identities` so daily/range accounting survives.
            sqlx::query("delete from activity_exchanges").execute(&mut *tx).await?.rows_affected()
        }
        ClearTarget::ActivityAll => {
            let a = sqlx::query("delete from activity_exchanges").execute(&mut *tx).await?.rows_affected();
            let b = sqlx::query("delete from activity_totals").execute(&mut *tx).await?.rows_affected();
            let c = sqlx::query("delete from activity_identities").execute(&mut *tx).await?.rows_affected();
            let _ = sqlx::query("delete from activity_session_edges").execute(&mut *tx).await?;
            a + b + c
        }
    };
    audit(
        &mut tx,
        actor,
        "providers.data.cleared",
        "provider_data",
        format!("{:?}", input.target),
        json!({ "rows_deleted": rows_deleted }),
    )
    .await?;
    tx.commit().await?;
    Ok(ClearDataOut { target: input.target, rows_deleted })
}

/// Build a bounded, sanitized diagnostic bundle preview for operator review before saving.
pub async fn preview_diagnostic_bundle(hub: &Hub, actor: &Actor, input: DiagnosticBundlePreviewIn) -> DResult<DiagnosticBundlePreview> {
    actor.require(OPERATOR)?;
    if input.exchange_ids.is_empty() || input.exchange_ids.len() > 50 {
        return Err(invalid("diagnostic_bounds", "select between 1 and 50 exchanges for a diagnostic bundle"));
    }
    let mut exchanges_out = Vec::new();
    let mut warnings = vec![
        "Review this bundle carefully before sharing: automated redaction removes known secret patterns, not arbitrary PII.".to_string(),
    ];
    let mut any_redacted = false;

    for id in &input.exchange_ids {
        let row: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object(
                'id', id,
                'provider_id', provider_id,
                'client', client,
                'requested_model', requested_model,
                'effective_model', effective_model,
                'effort', effort,
                'outcome', outcome,
                'status_code', status_code,
                'latency_ms', latency_ms,
                'ttft_ms', ttft_ms,
                'input_tokens', input_tokens,
                'output_tokens', output_tokens,
                'cache_read_tokens', cache_read_tokens,
                'cache_write_tokens', cache_write_tokens,
                'thinking_tokens', thinking_tokens,
                'capture_state', capture_state,
                'occurred_at', occurred_at,
                'error_message', case when $2 then error_message else null end
             ) from activity_exchanges where id = $1",
        )
        .bind(id)
        .bind(input.include_error_messages)
        .fetch_optional(&hub.db)
        .await?;

        let Some(mut entry) = row else {
            continue;
        };
        if input.include_transformed_bodies {
            let bodies: Vec<Value> = sqlx::query_scalar(
                "select payload from activity_captures where exchange_id = $1 and representation = 'transformed' order by created_at asc limit 5",
            )
            .bind(id)
            .fetch_all(&hub.db)
            .await?;
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("transformed_bodies".into(), Value::Array(bodies));
            }
        }
        let (clean, hit) = redact_json(&entry);
        any_redacted |= hit;
        exchanges_out.push(clean);
    }

    let mut proxy_logs = Vec::new();
    if input.include_proxy_logs {
        let logs: Vec<Value> = sqlx::query_scalar("select recent_logs from provider_routing_state").fetch_all(&hub.db).await?;
        for batch in logs {
            let (clean, hit) = redact_json(&batch);
            any_redacted |= hit;
            proxy_logs.push(clean);
        }
    }

    if any_redacted {
        warnings.push("One or more credential-like tokens or headers were redacted from the preview.".to_string());
    }
    let snapshot = json!({
        "schema_version": 1,
        "generated_at": Utc::now(),
        "include_error_messages": input.include_error_messages,
        "include_transformed_bodies": input.include_transformed_bodies,
        "exchanges": exchanges_out,
        "proxy_logs": proxy_logs,
    });
    let estimated_bytes = serde_json::to_vec(&snapshot).map(|b| b.len()).unwrap_or(0);
    Ok(DiagnosticBundlePreview {
        preview_id: Uuid::new_v4(),
        created_at: Utc::now(),
        exchange_count: exchanges_out.len(),
        redaction_warnings: warnings,
        estimated_bytes,
        snapshot,
    })
}

/// Export one session branch as an unencrypted, credential-sanitized JSONL archive.
pub async fn export_portable_archive(hub: &Hub, actor: &Actor, input: PortableArchiveRequest) -> DResult<PortableArchiveExport> {
    actor.require(OPERATOR)?;
    let session = input.session_id.trim();
    if session.is_empty() {
        return Err(invalid("missing_session_id", "session_id is required"));
    }

    let exchanges: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(e) from (
            select id, source_exchange_id, source, purpose, client, transport, session_id,
                   parent_session_id, parent_edge_kind, provider_id, account_id,
                   requested_model, effective_model, effort, outcome, status_code,
                   latency_ms, ttft_ms, input_tokens, output_tokens, cache_read_tokens,
                   cache_write_tokens, thinking_tokens, api_equivalent_cost_usd, capture_state, occurred_at
            from activity_exchanges
            where session_id = $1 or parent_session_id = $1
            order by occurred_at asc
            limit 500
         ) e",
    )
    .bind(session)
    .fetch_all(&hub.db)
    .await?;

    let mut lines = Vec::new();
    let mut warnings = vec!["Portable archive is unencrypted JSON Lines; known credentials are sanitized.".to_string()];
    let mut sanitized_any = false;
    let mut capture_count = 0usize;

    for ex in &exchanges {
        let (clean, hit) = redact_json(ex);
        sanitized_any |= hit;
        lines.push(serde_json::to_string(&json!({ "type": "exchange", "data": clean })).unwrap_or_default());

        if let Some(id_str) = ex.get("id").and_then(|v| v.as_str())
            && let Ok(ex_id) = Uuid::parse_str(id_str)
        {
            let attempts: Vec<Value> = sqlx::query_scalar(
                "select to_jsonb(a) from (
                    select id, exchange_id, attempt_index, provider_id, account_id, moved_from_account_id,
                           requested_model, effective_model, effort, status_code, outcome, latency_ms, ttft_ms,
                           input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, thinking_tokens,
                           capture_state, started_at, finished_at
                    from activity_attempts where exchange_id = $1 order by attempt_index asc
                 ) a",
            )
            .bind(ex_id)
            .fetch_all(&hub.db)
            .await?;
            for att in attempts {
                let (clean_a, hit_a) = redact_json(&att);
                sanitized_any |= hit_a;
                lines.push(serde_json::to_string(&json!({ "type": "attempt", "data": clean_a })).unwrap_or_default());
            }

            let captures: Vec<Value> = sqlx::query_scalar(
                "select jsonb_build_object('exchange_id', exchange_id, 'attempt_id', attempt_id, 'representation', representation, 'payload', payload)
                 from activity_captures
                 where exchange_id = $1 and ($2 or representation <> 'raw_wire')
                 order by created_at asc",
            )
            .bind(ex_id)
            .bind(input.include_raw_wire)
            .fetch_all(&hub.db)
            .await?;
            for cap in captures {
                let (clean_c, hit_c) = redact_json(&cap);
                sanitized_any |= hit_c;
                capture_count += 1;
                lines.push(serde_json::to_string(&json!({ "type": "capture", "data": clean_c })).unwrap_or_default());
            }
        }
    }

    if sanitized_any {
        warnings.push("Credential patterns were sanitized during export.".to_string());
    }
    let body = lines.join("\n");
    let digest = hash::sha256_hex(body.as_bytes());
    let manifest = json!({
        "schema_version": 1,
        "format": "fleetix-hub-portable-archive-jsonl",
        "session_id": session,
        "exported_at": Utc::now(),
        "exchange_count": exchanges.len(),
        "capture_count": capture_count,
        "include_raw_wire": input.include_raw_wire,
        "sha256": digest,
    });
    let mut full_lines = vec![serde_json::to_string(&json!({ "type": "manifest", "data": manifest })).unwrap_or_default()];
    if !body.is_empty() {
        full_lines.push(body);
    }

    Ok(PortableArchiveExport { manifest, warnings, jsonl: full_lines.join("\n") })
}

pub async fn get_clickhouse_status(hub: &Hub, actor: &Actor) -> DResult<ClickHouseStatus> {
    actor.require(crate::ANY_READER)?;
    let raw: Option<Value> =
        sqlx::query_scalar("select value from settings where key = 'provider_clickhouse_status'").fetch_optional(&hub.db).await?;
    Ok(raw.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
}

pub async fn set_clickhouse_config(hub: &Hub, actor: &Actor, config: ClickHouseConfig) -> DResult<ClickHouseStatus> {
    actor.require(OPERATOR)?;
    if config.enabled {
        let ep = config.endpoint.as_deref().unwrap_or("").trim();
        if !(ep.starts_with("https://") || ep.starts_with("http://127.0.0.1") || ep.starts_with("http://localhost")) {
            return Err(invalid("invalid_clickhouse_endpoint", "ClickHouse endpoint must be https:// or loopback http://"));
        }
    }
    let mut current = get_clickhouse_status(hub, actor).await?;
    current.config = config;
    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "insert into settings (key, value, updated_at) values ('provider_clickhouse_status', $1, now())
         on conflict (key) do update set value = excluded.value, updated_at = now()",
    )
    .bind(serde_json::to_value(&current).map_err(|e| DomainError::Internal(e.to_string()))?)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        "providers.clickhouse.configured",
        "provider_settings",
        "clickhouse",
        json!({ "enabled": current.config.enabled, "export_content": current.config.export_content }),
    )
    .await?;
    tx.commit().await?;
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_bearer_and_secret_keys() {
        let payload = json!({
            "authorization": "Bearer sk-ant-secret123456",
            "note": "header Bearer tok_abc123 failed then Bearer tok_def456 and ghp_first ghp_second",
            "nested": { "refresh_token": "rt_secret", "safe": "ok" }
        });
        let (clean, hit) = redact_json(&payload);
        assert!(hit);
        assert_eq!(clean["authorization"], "<redacted>");
        assert_eq!(clean["nested"]["refresh_token"], "<redacted>");
        assert_eq!(clean["nested"]["safe"], "ok");
        let note = clean["note"].as_str().unwrap();
        assert!(!note.contains("tok_abc123"));
        assert!(!note.contains("tok_def456"));
        assert!(!note.contains("first"));
        assert!(!note.contains("second"));
    }
}
