//! Spillway-parity Activity engine: ingestion & source reconciliation,
//! Overview, Compare (list-price & task comparison), Requests & Inspector,
//! Session Traces (own vs subtotal + conflict/cycle detection), Drift & Canaries,
//! Totals (durable hourly aggregates), Thinking analysis & tests, and Evals.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use contracts::providers::*;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{capture, pricing};
use crate::{ANY_READER, Actor, DResult, DomainError, Hub, OPERATOR, OPERATOR_OR_COLLECTOR, audit, invalid};

/// Ingest a batch of activity exchanges and attempts from a proxy or local log collector.
/// Reconciles by `(source_host, source_user, source_exchange_id)` in `activity_identities`
/// so local imports and proxy streams never double-count historical totals.
pub async fn ingest_activity(hub: &Hub, actor: &Actor, batch: ActivityIngestBatch) -> DResult<ActivityIngestAck> {
    actor.require(OPERATOR_OR_COLLECTOR)?;
    super::enforce_collector_source(hub, actor, &batch.source_host, &batch.source_user).await?;
    let providers: BTreeSet<&str> = batch.exchanges.iter().map(|ex| ex.provider_id.as_str()).collect();
    for provider_id in providers {
        super::enforce_collector_scope(hub, actor, &batch.source_host, &batch.source_user, provider_id).await?;
    }
    let catalog = pricing::default_catalog();
    let capture_policy = capture::get_capture_policy(hub, &super::internal_operator("activity-ingest")).await.unwrap_or_default();

    let mut tx = hub.db.begin().await?;
    let mut accepted = 0usize;
    let mut deduplicated = 0usize;

    for ex in &batch.exchanges {
        let exists: bool = sqlx::query_scalar(
            "select exists(
                select 1 from activity_identities
                where source_host = $1 and source_user = $2 and source_exchange_id = $3
             )",
        )
        .bind(&batch.source_host)
        .bind(&batch.source_user)
        .bind(&ex.source_exchange_id)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            deduplicated += 1;
            continue;
        }

        let exchange_id = Uuid::new_v4();
        let price = pricing::lookup_price(&ex.effective_model, &catalog);
        let cost = price.map(|p| {
            pricing::estimate_cost(
                pricing::TokenCounts {
                    input_tokens: ex.input_tokens,
                    output_tokens: ex.output_tokens,
                    cache_read_tokens: ex.cache_read_tokens,
                    cache_write_tokens: ex.cache_write_tokens,
                    thinking_tokens: ex.thinking_tokens,
                },
                p,
            )
            .total_usd
        });

        let source_str = enum_str(ex.source, "proxy");
        let purpose_str = enum_str(ex.purpose, "ordinary");
        let capture_str = enum_str(ex.capture_state, "none");
        let edge_str = ex.parent_edge_kind.map(|k| enum_str(k, "subagent"));
        let moved_from = ex.attempts.iter().find_map(|a| a.moved_from_account_id.clone());
        let safe_err = ex.error_message.as_deref().map(|e| capture::redact_text(e).0);

        sqlx::query(
            "insert into activity_exchanges
                (id, source_host, source_user, source_exchange_id, source, purpose, client, client_version,
                 transport, session_id, parent_session_id, parent_edge_kind, provider_id, account_id,
                 moved_from_account_id, requested_model, effective_model, effort, outcome, status_code,
                 error_message, latency_ms, ttft_ms, input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, thinking_tokens, fast_mode, api_equivalent_cost_usd, capture_state,
                 searchable_text, occurred_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8,
                     $9, $10, $11, $12, $13, $14,
                     $15, $16, $17, $18, $19, $20,
                     $21, $22, $23, $24, $25, $26,
                     $27, $28, $29, $30, $31,
                     $32, $33)",
        )
        .bind(exchange_id)
        .bind(&batch.source_host)
        .bind(&batch.source_user)
        .bind(&ex.source_exchange_id)
        .bind(&source_str)
        .bind(&purpose_str)
        .bind(&ex.client)
        .bind(&ex.client_version)
        .bind(&ex.transport)
        .bind(&ex.session_id)
        .bind(&ex.parent_session_id)
        .bind(&edge_str)
        .bind(&ex.provider_id)
        .bind(&ex.account_id)
        .bind(&moved_from)
        .bind(&ex.requested_model)
        .bind(&ex.effective_model)
        .bind(&ex.effort)
        .bind(&ex.outcome)
        .bind(ex.status_code)
        .bind(&safe_err)
        .bind(ex.latency_ms)
        .bind(ex.ttft_ms)
        .bind(ex.input_tokens)
        .bind(ex.output_tokens)
        .bind(ex.cache_read_tokens)
        .bind(ex.cache_write_tokens)
        .bind(ex.thinking_tokens)
        .bind(ex.fast_mode)
        .bind(cost)
        .bind(&capture_str)
        .bind(if capture_policy.capture_bodies_enabled { ex.searchable_text.as_deref() } else { None })
        .bind(ex.occurred_at)
        .execute(&mut *tx)
        .await?;

        for att in &ex.attempts {
            let attempt_id = Uuid::new_v4();
            let att_price = pricing::lookup_price(&att.effective_model, &catalog);
            let att_cost = att_price.map(|p| {
                pricing::estimate_cost(
                    pricing::TokenCounts {
                        input_tokens: att.input_tokens,
                        output_tokens: att.output_tokens,
                        cache_read_tokens: att.cache_read_tokens,
                        cache_write_tokens: att.cache_write_tokens,
                        thinking_tokens: att.thinking_tokens,
                    },
                    p,
                )
                .total_usd
            });
            let att_cap = enum_str(att.capture_state, "none");
            let att_err = att.error_message.as_deref().map(|e| capture::redact_text(e).0);
            sqlx::query(
                "insert into activity_attempts
                    (id, exchange_id, source_attempt_id, attempt_index, provider_id, account_id,
                     moved_from_account_id, requested_model, effective_model, effort, status_code,
                     outcome, error_message, latency_ms, ttft_ms, input_tokens, output_tokens,
                     cache_read_tokens, cache_write_tokens, thinking_tokens, fast_mode,
                     api_equivalent_cost_usd, capture_state, started_at, finished_at)
                 values ($1, $2, $3, $4, $5, $6,
                         $7, $8, $9, $10, $11,
                         $12, $13, $14, $15, $16, $17,
                         $18, $19, $20, $21,
                         $22, $23, $24, $25)",
            )
            .bind(attempt_id)
            .bind(exchange_id)
            .bind(&att.source_attempt_id)
            .bind(att.attempt_index)
            .bind(&att.provider_id)
            .bind(&att.account_id)
            .bind(&att.moved_from_account_id)
            .bind(&att.requested_model)
            .bind(&att.effective_model)
            .bind(&att.effort)
            .bind(att.status_code)
            .bind(&att.outcome)
            .bind(&att_err)
            .bind(att.latency_ms)
            .bind(att.ttft_ms)
            .bind(att.input_tokens)
            .bind(att.output_tokens)
            .bind(att.cache_read_tokens)
            .bind(att.cache_write_tokens)
            .bind(att.thinking_tokens)
            .bind(att.fast_mode)
            .bind(att_cost)
            .bind(&att_cap)
            .bind(att.started_at)
            .bind(att.finished_at)
            .execute(&mut *tx)
            .await?;

            // Only store captured bodies when opt-in capture is explicitly enabled.
            if capture_policy.capture_bodies_enabled
                && let Some(body) = &att.captured_body
            {
                let (clean_body, redacted) = capture::redact_json(body);
                let size_bytes = serde_json::to_vec(&clean_body).map(|b| b.len() as i64).unwrap_or(0);
                let expires_at = Utc::now() + Duration::days(capture_policy.body_retention_days as i64);
                sqlx::query(
                    "insert into activity_captures
                        (id, exchange_id, attempt_id, representation, redacted, size_bytes, payload, expires_at)
                     values ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(Uuid::new_v4())
                .bind(exchange_id)
                .bind(attempt_id)
                .bind(&att_cap)
                .bind(redacted)
                .bind(size_bytes)
                .bind(&clean_body)
                .bind(expires_at)
                .execute(&mut *tx)
                .await?;
            }
        }

        if let (Some(child), Some(parent)) = (&ex.session_id, &ex.parent_session_id) {
            sqlx::query(
                "insert into activity_session_edges (child_session_id, parent_session_id, edge_kind)
                 values ($1, $2, $3) on conflict do nothing",
            )
            .bind(child)
            .bind(parent)
            .bind(edge_str.as_deref().unwrap_or("subagent"))
            .execute(&mut *tx)
            .await?;
        }

        let hour_bucket = truncate_hour(ex.occurred_at);
        sqlx::query(
            "insert into activity_identities
                (source_host, source_user, source_exchange_id, canonical_exchange_id, hour_bucket, contribution)
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&batch.source_host)
        .bind(&batch.source_user)
        .bind(&ex.source_exchange_id)
        .bind(exchange_id)
        .bind(hour_bucket)
        .bind(json!({
            "cost_usd": cost,
            "input_tokens": ex.input_tokens,
            "output_tokens": ex.output_tokens,
            "purpose": purpose_str
        }))
        .execute(&mut *tx)
        .await?;

        // Window starters, canaries, thinking tests and eval replays stay outside ordinary activity totals.
        if ex.purpose == ActivityPurpose::Ordinary {
            let is_fail = i64::from(!is_success_outcome(&ex.outcome));
            let is_unpriced = i64::from(cost.is_none());
            sqlx::query(
                "insert into activity_totals
                    (hour_bucket, source, provider_id, account_id, client, effective_model, effort,
                     requests, failures, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                     thinking_tokens, api_equivalent_cost_usd, unpriced_requests)
                 values ($1, $2, $3, $4, $5, $6, $7,
                         1, $8, $9, $10, $11, $12,
                         $13, $14, $15)
                 on conflict (hour_bucket, source, provider_id, account_id, client, effective_model, effort)
                 do update set
                    requests = activity_totals.requests + 1,
                    failures = activity_totals.failures + excluded.failures,
                    input_tokens = activity_totals.input_tokens + excluded.input_tokens,
                    output_tokens = activity_totals.output_tokens + excluded.output_tokens,
                    cache_read_tokens = activity_totals.cache_read_tokens + excluded.cache_read_tokens,
                    cache_write_tokens = activity_totals.cache_write_tokens + excluded.cache_write_tokens,
                    thinking_tokens = activity_totals.thinking_tokens + excluded.thinking_tokens,
                    api_equivalent_cost_usd = activity_totals.api_equivalent_cost_usd + excluded.api_equivalent_cost_usd,
                    unpriced_requests = activity_totals.unpriced_requests + excluded.unpriced_requests",
            )
            .bind(hour_bucket)
            .bind(&source_str)
            .bind(&ex.provider_id)
            .bind(ex.account_id.as_deref().unwrap_or(""))
            .bind(&ex.client)
            .bind(&ex.effective_model)
            .bind(ex.effort.as_deref().unwrap_or(""))
            .bind(is_fail)
            .bind(ex.input_tokens)
            .bind(ex.output_tokens)
            .bind(ex.cache_read_tokens)
            .bind(ex.cache_write_tokens)
            .bind(ex.thinking_tokens)
            .bind(cost.unwrap_or(0.0))
            .bind(is_unpriced)
            .execute(&mut *tx)
            .await?;
        }

        accepted += 1;
    }

    tx.commit().await?;
    Ok(ActivityIngestAck { accepted, deduplicated, recorded_at: Utc::now() })
}

fn is_success_outcome(outcome: &str) -> bool {
    matches!(outcome, "ok" | "succeeded" | "success")
}

fn parse_cursor(raw: &str) -> DResult<(DateTime<Utc>, Option<Uuid>)> {
    if let Some((ts, id_str)) = raw.split_once('|') {
        let occurred_at = DateTime::parse_from_rfc3339(ts)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|_| invalid("invalid_cursor", "cursor timestamp must be RFC3339"))?;
        let id = Uuid::parse_str(id_str).map_err(|_| invalid("invalid_cursor", "cursor id must be a UUID"))?;
        Ok((occurred_at, Some(id)))
    } else {
        let occurred_at = DateTime::parse_from_rfc3339(raw)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|_| invalid("invalid_cursor", "cursor must be RFC3339 or '<rfc3339>|<uuid>'"))?;
        Ok((occurred_at, None))
    }
}

fn format_cursor(occurred_at: DateTime<Utc>, id: Uuid) -> String {
    format!("{}|{id}", occurred_at.to_rfc3339())
}

fn enum_str<T: serde::Serialize>(val: T, fallback: &str) -> String {
    serde_json::to_value(val).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_else(|| fallback.to_string())
}

fn truncate_hour(dt: DateTime<Utc>) -> DateTime<Utc> {
    dt.with_minute(0).and_then(|d| d.with_second(0)).and_then(|d| d.with_nanosecond(0)).unwrap_or(dt)
}

#[derive(sqlx::FromRow)]
struct ExchangeRow {
    id: Uuid,
    source_exchange_id: String,
    source: String,
    purpose: String,
    client: String,
    client_version: Option<String>,
    transport: Option<String>,
    session_id: Option<String>,
    parent_session_id: Option<String>,
    #[allow(dead_code)]
    parent_edge_kind: Option<String>,
    provider_id: String,
    account_id: Option<String>,
    moved_from_account_id: Option<String>,
    requested_model: String,
    effective_model: String,
    effort: Option<String>,
    outcome: String,
    status_code: Option<i32>,
    error_message: Option<String>,
    latency_ms: Option<i64>,
    ttft_ms: Option<i64>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    thinking_tokens: i64,
    fast_mode: bool,
    api_equivalent_cost_usd: Option<f64>,
    capture_state: String,
    occurred_at: DateTime<Utc>,
    attempt_count: i64,
}

impl ExchangeRow {
    fn to_contract(&self) -> RequestRow {
        RequestRow {
            id: self.id,
            source_exchange_id: self.source_exchange_id.clone(),
            source: parse_enum(&self.source, ActivitySource::Proxy),
            purpose: parse_enum(&self.purpose, ActivityPurpose::Ordinary),
            provider_id: self.provider_id.clone(),
            account_id: self.account_id.clone(),
            client: self.client.clone(),
            transport: self.transport.clone(),
            session_id: self.session_id.clone(),
            requested_model: self.requested_model.clone(),
            effective_model: self.effective_model.clone(),
            effort: self.effort.clone(),
            outcome: self.outcome.clone(),
            status_code: self.status_code,
            error_message: self.error_message.clone(),
            latency_ms: self.latency_ms,
            ttft_ms: self.ttft_ms,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            thinking_tokens: self.thinking_tokens,
            api_equivalent_cost_usd: self.api_equivalent_cost_usd,
            moved_from_account_id: self.moved_from_account_id.clone(),
            attempt_count: self.attempt_count.max(1) as u32,
            capture_state: parse_enum(&self.capture_state, CaptureState::None),
            occurred_at: self.occurred_at,
        }
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(s: &str, default: T) -> T {
    serde_json::from_value(Value::String(s.to_string())).unwrap_or(default)
}

async fn query_exchanges(db: &sqlx::PgPool, scope: &ActivityScope, ordinary_only: bool, limit: Option<i64>) -> DResult<Vec<ExchangeRow>> {
    let from = scope.from.unwrap_or_else(|| Utc::now() - Duration::days(30));
    let to = scope.to.unwrap_or_else(Utc::now);
    let source_filter: Option<String> = match scope.source {
        None | Some(ActivitySource::All) => None,
        Some(other) => Some(enum_str(other, "proxy")),
    };
    let (cursor_ts, cursor_id) = match scope.cursor.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            let (ts, id) = parse_cursor(raw)?;
            (Some(ts), id)
        }
        None => (None, None),
    };
    let rows = sqlx::query_as::<_, ExchangeRow>(
        "select e.id, e.source_exchange_id, e.source, e.purpose, e.client, e.client_version, e.transport,
                e.session_id, e.parent_session_id, e.parent_edge_kind, e.provider_id, e.account_id,
                e.moved_from_account_id, e.requested_model, e.effective_model, e.effort, e.outcome,
                e.status_code, e.error_message, e.latency_ms, e.ttft_ms, e.input_tokens, e.output_tokens,
                e.cache_read_tokens, e.cache_write_tokens, e.thinking_tokens, e.fast_mode,
                e.api_equivalent_cost_usd, e.capture_state, e.occurred_at,
                (select count(*) from activity_attempts a where a.exchange_id = e.id) as attempt_count
         from activity_exchanges e
         where e.occurred_at >= $1 and e.occurred_at <= $2
           and (not $3 or e.purpose = 'ordinary')
           and ($4::text is null or e.source = $4)
           and ($5::text is null or e.client = $5)
           and ($6::text is null or e.provider_id = $6)
           and ($7::text is null or e.account_id = $7)
           and ($8::text is null or e.effective_model ilike '%' || $8 || '%')
           and ($9::text is null or e.effort = $9)
           and (
                $10::text is null
                or ($10 = 'failed' and e.outcome not in ('ok', 'succeeded', 'success'))
                or ($10 in ('ok', 'succeeded', 'success') and e.outcome in ('ok', 'succeeded', 'success'))
                or e.outcome = $10
           )
           and ($11::text is null or e.session_id ilike '%' || $11 || '%'
                or e.effective_model ilike '%' || $11 || '%'
                or coalesce(e.searchable_text, '') ilike '%' || $11 || '%')
           and (
                $12::timestamptz is null
                or e.occurred_at < $12
                or ($13::uuid is not null and e.occurred_at = $12 and e.id < $13)
           )
         order by e.occurred_at desc, e.id desc
         limit $14",
    )
    .bind(from)
    .bind(to)
    .bind(ordinary_only)
    .bind(&source_filter)
    .bind(&scope.client)
    .bind(&scope.provider_id)
    .bind(&scope.account_id)
    .bind(&scope.model)
    .bind(&scope.effort)
    .bind(&scope.outcome)
    .bind(&scope.search)
    .bind(cursor_ts)
    .bind(cursor_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

// ── 1. Overview ─────────────────────────────────────────────────────────────

pub async fn overview(hub: &Hub, actor: &Actor, scope: ActivityScope) -> DResult<ActivityOverview> {
    actor.require(ANY_READER)?;
    let rows = query_exchanges(&hub.db, &scope, true, None).await?;
    let catalog = pricing::default_catalog();
    let policy = capture::get_capture_policy(hub, actor).await.unwrap_or_default();
    let tz_offset = Duration::minutes(scope.tz_offset_mins.unwrap_or(0) as i64);

    let mut cost_usd = 0.0;
    let mut unpriced_requests = 0u64;
    let mut unpriced_models = BTreeSet::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut cache_write_tokens = 0u64;
    let mut thinking_tokens = 0u64;
    let mut failures = 0u64;
    let mut cache_savings_usd = 0.0;
    let mut matched_proxy = 0u64;
    let mut bypassed_local = 0u64;

    let mut trend_map: BTreeMap<DateTime<Utc>, ActivityTrendPoint> = BTreeMap::new();
    let mut heatmap_map: BTreeMap<(u8, u8), HeatmapCell> = BTreeMap::new();
    let mut rhythm_map: BTreeMap<u8, RhythmBucket> =
        (0..24).map(|h| (h, RhythmBucket { hour: h, requests: 0, tokens: 0, api_equivalent_cost_usd: 0.0 })).collect();
    let mut by_model: BTreeMap<String, BreakdownRow> = BTreeMap::new();
    let mut by_account: BTreeMap<String, BreakdownRow> = BTreeMap::new();
    let mut by_client: BTreeMap<String, BreakdownRow> = BTreeMap::new();
    let mut by_model_effort: BTreeMap<String, BreakdownRow> = BTreeMap::new();

    for r in &rows {
        let is_fail = u64::from(!is_success_outcome(&r.outcome));
        failures += is_fail;
        let inp = r.input_tokens.max(0) as u64;
        let out = r.output_tokens.max(0) as u64;
        let cr = r.cache_read_tokens.max(0) as u64;
        let cw = r.cache_write_tokens.max(0) as u64;
        let th = r.thinking_tokens.max(0) as u64;
        input_tokens += inp;
        output_tokens += out;
        cache_read_tokens += cr;
        cache_write_tokens += cw;
        thinking_tokens += th;

        if r.source == "proxy" {
            matched_proxy += 1;
        } else {
            bypassed_local += 1;
        }

        let row_cost = match r.api_equivalent_cost_usd {
            Some(c) => {
                if let Some(p) = pricing::lookup_price(&r.effective_model, &catalog) {
                    let est = pricing::estimate_cost(
                        pricing::TokenCounts {
                            input_tokens: r.input_tokens,
                            output_tokens: r.output_tokens,
                            cache_read_tokens: r.cache_read_tokens,
                            cache_write_tokens: r.cache_write_tokens,
                            thinking_tokens: r.thinking_tokens,
                        },
                        p,
                    );
                    cache_savings_usd += est.cache_savings_usd;
                }
                cost_usd += c;
                c
            }
            None => {
                unpriced_requests += 1;
                unpriced_models.insert(r.effective_model.clone());
                0.0
            }
        };

        let day = r.occurred_at.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let tp = trend_map.entry(day).or_insert_with(|| ActivityTrendPoint {
            bucket_start: day,
            label: day.format("%m-%d").to_string(),
            requests: 0,
            failures: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            thinking_tokens: 0,
            api_equivalent_cost_usd: 0.0,
        });
        tp.requests += 1;
        tp.failures += is_fail;
        tp.input_tokens += inp;
        tp.output_tokens += out;
        tp.cache_read_tokens += cr;
        tp.thinking_tokens += th;
        tp.api_equivalent_cost_usd += row_cost;

        let local = r.occurred_at + tz_offset;
        let dow = local.weekday().num_days_from_monday() as u8;
        let hour = local.hour() as u8;
        let hc = heatmap_map.entry((dow, hour)).or_insert(HeatmapCell {
            day_of_week: dow,
            hour,
            requests: 0,
            tokens: 0,
            api_equivalent_cost_usd: 0.0,
        });
        hc.requests += 1;
        hc.tokens += inp + out;
        hc.api_equivalent_cost_usd += row_cost;

        if let Some(rb) = rhythm_map.get_mut(&hour) {
            rb.requests += 1;
            rb.tokens += inp + out;
            rb.api_equivalent_cost_usd += row_cost;
        }

        let unpriced_inc = u64::from(r.api_equivalent_cost_usd.is_none());
        add_breakdown(&mut by_model, &r.effective_model, &r.effective_model, is_fail, inp + out, out, cr, th, row_cost, unpriced_inc);
        let acct_key = format!("{}:{}", r.provider_id, r.account_id.as_deref().unwrap_or("unassigned"));
        add_breakdown(&mut by_account, &acct_key, &acct_key, is_fail, inp + out, out, cr, th, row_cost, unpriced_inc);
        add_breakdown(&mut by_client, &r.client, &r.client, is_fail, inp + out, out, cr, th, row_cost, unpriced_inc);
        let me_key = match &r.effort {
            Some(e) if !e.is_empty() => format!("{} ({e})", r.effective_model),
            _ => r.effective_model.clone(),
        };
        add_breakdown(&mut by_model_effort, &me_key, &me_key, is_fail, inp + out, out, cr, th, row_cost, unpriced_inc);
    }

    let cache_hit_rate = if input_tokens > 0 { (cache_read_tokens as f64 / input_tokens as f64).clamp(0.0, 1.0) } else { 0.0 };

    Ok(ActivityOverview {
        api_equivalent_cost_usd: cost_usd,
        unpriced_requests,
        unpriced_models: unpriced_models.into_iter().collect(),
        price_as_of: pricing::PRICE_AS_OF.to_string(),
        cache_write_assumption: pricing::CACHE_WRITE_ASSUMPTION.to_string(),
        total_tokens: input_tokens + output_tokens,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        thinking_tokens,
        requests: rows.len() as u64,
        failures,
        cache_hit_rate,
        cache_savings_usd,
        matched_proxy_requests: matched_proxy,
        bypassed_local_requests: bypassed_local,
        trend: trend_map.into_values().collect(),
        heatmap: heatmap_map.into_values().collect(),
        rhythm: rhythm_map.into_values().collect(),
        by_model: sort_breakdown(by_model),
        by_account: sort_breakdown(by_account),
        by_client: sort_breakdown(by_client),
        by_model_effort: sort_breakdown(by_model_effort),
        board_tiles: default_overview_tiles(),
        detail_retention_days: policy.detail_retention_days,
    })
}

#[allow(clippy::too_many_arguments)]
fn add_breakdown(
    map: &mut BTreeMap<String, BreakdownRow>,
    key: &str,
    label: &str,
    failures: u64,
    total_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    thinking: u64,
    cost: f64,
    unpriced: u64,
) {
    let e = map.entry(key.to_string()).or_insert_with(|| BreakdownRow {
        key: key.to_string(),
        label: label.to_string(),
        requests: 0,
        failures: 0,
        total_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        thinking_tokens: 0,
        api_equivalent_cost_usd: 0.0,
        unpriced_requests: 0,
    });
    e.requests += 1;
    e.failures += failures;
    e.total_tokens += total_tokens;
    e.output_tokens += output_tokens;
    e.cache_read_tokens += cache_read;
    e.thinking_tokens += thinking;
    e.api_equivalent_cost_usd += cost;
    e.unpriced_requests += unpriced;
}

fn sort_breakdown(map: BTreeMap<String, BreakdownRow>) -> Vec<BreakdownRow> {
    let mut v: Vec<_> = map.into_values().collect();
    v.sort_by(|a, b| {
        b.api_equivalent_cost_usd
            .partial_cmp(&a.api_equivalent_cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.requests.cmp(&a.requests))
    });
    v
}

fn default_overview_tiles() -> Vec<BoardTile> {
    vec![
        BoardTile { id: "trend".into(), kind: "trend".into(), metric: "cost".into(), span: 2, visible: true },
        BoardTile { id: "cache".into(), kind: "cache".into(), metric: "cache_hit".into(), span: 1, visible: true },
        BoardTile { id: "heatmap".into(), kind: "heatmap".into(), metric: "requests".into(), span: 2, visible: true },
        BoardTile { id: "rhythm".into(), kind: "rhythm".into(), metric: "requests".into(), span: 1, visible: true },
        BoardTile { id: "models".into(), kind: "models".into(), metric: "cost".into(), span: 1, visible: true },
        BoardTile { id: "accounts".into(), kind: "accounts".into(), metric: "cost".into(), span: 1, visible: true },
        BoardTile { id: "clients".into(), kind: "clients".into(), metric: "requests".into(), span: 1, visible: true },
        BoardTile { id: "compare".into(), kind: "compare".into(), metric: "cost".into(), span: 3, visible: true },
    ]
}

// ── 2. Compare (and in-app list price table) ────────────────────────────────

pub async fn compare(hub: &Hub, actor: &Actor, scope: ActivityScope) -> DResult<CompareReport> {
    actor.require(ANY_READER)?;
    let rows = query_exchanges(&hub.db, &scope, true, None).await?;
    let catalog = pricing::default_catalog();

    // Group exchanges by (model, effort) and track root-session tasks & subagent costs.
    let mut by_key: BTreeMap<String, Vec<&ExchangeRow>> = BTreeMap::new();
    let mut session_models: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in &rows {
        let key = match &r.effort {
            Some(e) if !e.is_empty() => format!("{} · {e}", r.effective_model),
            _ => r.effective_model.clone(),
        };
        by_key.entry(key.clone()).or_default().push(r);
        if let Some(sid) = &r.session_id {
            session_models.entry(sid.clone()).or_default().insert(key);
        }
    }

    let mut out = Vec::new();
    for (key, group) in by_key {
        let first = group[0];
        let reqs = group.len() as u64;
        let fails = group.iter().filter(|r| !is_success_outcome(&r.outcome)).count() as u64;
        let total_cost: f64 = group.iter().filter_map(|r| r.api_equivalent_cost_usd).sum();
        let failed_cost: f64 = group.iter().filter(|r| !is_success_outcome(&r.outcome)).filter_map(|r| r.api_equivalent_cost_usd).sum();
        let inp: i64 = group.iter().map(|r| r.input_tokens.max(0)).sum();
        let out_tok: i64 = group.iter().map(|r| r.output_tokens.max(0)).sum();
        let cr: i64 = group.iter().map(|r| r.cache_read_tokens.max(0)).sum();
        let th: i64 = group.iter().map(|r| r.thinking_tokens.max(0)).sum();
        let fast_cnt = group.iter().filter(|r| r.fast_mode).count() as u64;
        let subagent_cost: f64 = group.iter().filter(|r| r.parent_session_id.is_some()).filter_map(|r| r.api_equivalent_cost_usd).sum();

        // Root task metrics (sessions with no parent_session_id).
        let mut task_costs: BTreeMap<String, (f64, u64, u64)> = BTreeMap::new();
        for r in &group {
            if r.parent_session_id.is_none()
                && let Some(sid) = &r.session_id
            {
                let e = task_costs.entry(sid.clone()).or_insert((0.0, 0, 0));
                e.0 += r.api_equivalent_cost_usd.unwrap_or(0.0);
                e.1 += (r.input_tokens.max(0) + r.output_tokens.max(0)) as u64;
                e.2 += 1;
            }
        }
        let root_tasks = task_costs.len() as u64;
        let mixed_tasks = task_costs.keys().filter(|sid| session_models.get(*sid).is_some_and(|m| m.len() > 1)).count() as u64;
        let mut costs_vec: Vec<f64> = task_costs.values().map(|v| v.0).collect();
        costs_vec.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut toks_vec: Vec<u64> = task_costs.values().map(|v| v.1).collect();
        toks_vec.sort();
        let mut reqs_vec: Vec<u64> = task_costs.values().map(|v| v.2).collect();
        reqs_vec.sort();

        let total_toks = (inp + out_tok) as f64;
        out.push(CompareRow {
            key,
            model: first.effective_model.clone(),
            effort: first.effort.clone(),
            price: pricing::lookup_price(&first.effective_model, &catalog).cloned(),
            api_equivalent_cost_usd: total_cost,
            requests: reqs,
            failures: fails,
            mean_cost_per_request: (reqs > 0).then_some(total_cost / reqs as f64),
            cost_per_m_tokens: (total_toks > 0.0).then_some(total_cost * 1_000_000.0 / total_toks),
            cache_hit_pct: if inp > 0 { (cr as f64 / inp as f64) * 100.0 } else { 0.0 },
            failed_cost_share_pct: if total_cost > 0.0 { (failed_cost / total_cost) * 100.0 } else { 0.0 },
            thinking_output_share_pct: if out_tok > 0 { (th as f64 / out_tok as f64) * 100.0 } else { 0.0 },
            fast_flex_share_pct: if reqs > 0 { (fast_cnt as f64 / reqs as f64) * 100.0 } else { 0.0 },
            root_tasks,
            median_task_cost: percentile_f64(&costs_vec, 0.5),
            mean_task_cost: (!costs_vec.is_empty()).then_some(costs_vec.iter().sum::<f64>() / costs_vec.len() as f64),
            p90_task_cost: percentile_f64(&costs_vec, 0.9),
            median_tokens_per_task: percentile_u64(&toks_vec, 0.5),
            median_requests_per_task: percentile_u64(&reqs_vec, 0.5),
            mixed_task_share_pct: if root_tasks > 0 { (mixed_tasks as f64 / root_tasks as f64) * 100.0 } else { 0.0 },
            other_model_share_pct: 0.0,
            subagent_cost_usd: subagent_cost,
            baseline_cost_multiple: None,
            baseline_req_cost_multiple: None,
            baseline_cache_pp_delta: None,
            detail_coverage_pct: 100.0,
        });
    }

    out.sort_by(|a, b| b.api_equivalent_cost_usd.partial_cmp(&a.api_equivalent_cost_usd).unwrap_or(std::cmp::Ordering::Equal));

    let baseline_key = scope.baseline_key.clone().or_else(|| out.first().map(|r| r.key.clone()));
    if let Some(ref bkey) = baseline_key
        && let Some(base) = out.iter().find(|r| &r.key == bkey).cloned()
    {
        for row in &mut out {
            row.baseline_cost_multiple =
                (base.api_equivalent_cost_usd > 0.0).then_some(row.api_equivalent_cost_usd / base.api_equivalent_cost_usd);
            row.baseline_req_cost_multiple = match (row.mean_cost_per_request, base.mean_cost_per_request) {
                (Some(a), Some(b)) if b > 0.0 => Some(a / b),
                _ => None,
            };
            row.baseline_cache_pp_delta = Some(row.cache_hit_pct - base.cache_hit_pct);
        }
    }

    Ok(CompareReport { baseline_key, price_as_of: pricing::PRICE_AS_OF.to_string(), rows: out, price_catalog: catalog })
}

fn percentile_f64(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted.get(idx).copied()
}

fn percentile_u64(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted.get(idx).copied()
}

// ── 3. Requests & Inspector ─────────────────────────────────────────────────

pub async fn requests(hub: &Hub, actor: &Actor, scope: ActivityScope) -> DResult<RequestPage> {
    actor.require(ANY_READER)?;
    let limit = scope.limit.unwrap_or(100).clamp(1, 500) as i64;
    let rows = query_exchanges(&hub.db, &scope, false, Some(limit + 1)).await?;
    let has_more = rows.len() as i64 > limit;
    let items: Vec<RequestRow> = rows.iter().take(limit as usize).map(ExchangeRow::to_contract).collect();
    let next_cursor = if has_more { items.last().map(|r| format_cursor(r.occurred_at, r.id)) } else { None };
    Ok(RequestPage { total_matched: items.len() as u64, items, next_cursor })
}

pub async fn request_detail(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<RequestDetail> {
    actor.require(ANY_READER)?;
    let row = sqlx::query_as::<_, ExchangeRow>(
        "select e.id, e.source_exchange_id, e.source, e.purpose, e.client, e.client_version, e.transport,
                e.session_id, e.parent_session_id, e.parent_edge_kind, e.provider_id, e.account_id,
                e.moved_from_account_id, e.requested_model, e.effective_model, e.effort, e.outcome,
                e.status_code, e.error_message, e.latency_ms, e.ttft_ms, e.input_tokens, e.output_tokens,
                e.cache_read_tokens, e.cache_write_tokens, e.thinking_tokens, e.fast_mode,
                e.api_equivalent_cost_usd, e.capture_state, e.occurred_at,
                (select count(*) from activity_attempts a where a.exchange_id = e.id) as attempt_count
         from activity_exchanges e where e.id = $1",
    )
    .bind(id)
    .fetch_optional(&hub.db)
    .await?
    .ok_or(DomainError::NotFound("activity request"))?;

    let att_rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(a) from (
            select id, attempt_index, provider_id, account_id, moved_from_account_id,
                   requested_model, effective_model, effort, status_code, outcome, error_message,
                   latency_ms, ttft_ms, input_tokens, output_tokens, cache_read_tokens,
                   cache_write_tokens, thinking_tokens, api_equivalent_cost_usd, capture_state,
                   started_at, finished_at
            from activity_attempts where exchange_id = $1 order by attempt_index asc
         ) a",
    )
    .bind(id)
    .fetch_all(&hub.db)
    .await?;
    let attempts: Vec<AttemptDetail> = crate::decode(att_rows)?;
    let capture_available: bool =
        sqlx::query_scalar("select exists(select 1 from activity_captures where exchange_id = $1)").bind(id).fetch_one(&hub.db).await?;
    let capture_gap_reason = if capture_available {
        None
    } else if row.capture_state == "evicted" {
        Some("Captured body expired or was evicted by retention budget.".into())
    } else {
        Some("Body capture was disabled or unavailable for this exchange.".into())
    };
    Ok(RequestDetail { exchange: row.to_contract(), attempts, capture_available, capture_gap_reason })
}

/// Explicitly gated read of a captured request/response body (operator only).
pub async fn request_capture(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<CapturedBodyView> {
    actor.require(OPERATOR)?;
    let row: Option<Value> = sqlx::query_scalar(
        "select to_jsonb(c) from (
            select exchange_id, attempt_id, representation, redacted, size_bytes, payload
            from activity_captures where exchange_id = $1 order by created_at desc limit 1
         ) c",
    )
    .bind(id)
    .fetch_optional(&hub.db)
    .await?;
    let row = row.ok_or(DomainError::NotFound("captured body"))?;
    crate::decode_one(row)
}

// ── 4. Session Traces ───────────────────────────────────────────────────────

pub async fn traces(hub: &Hub, actor: &Actor, scope: ActivityScope) -> DResult<TracePage> {
    actor.require(ANY_READER)?;
    let rows = query_exchanges(&hub.db, &scope, false, None).await?;
    let edges: Vec<(String, String, String)> =
        sqlx::query_as("select child_session_id, parent_session_id, edge_kind from activity_session_edges").fetch_all(&hub.db).await?;

    let mut unassigned = 0u64;
    let mut by_session: BTreeMap<String, Vec<&ExchangeRow>> = BTreeMap::new();
    for r in &rows {
        match &r.session_id {
            Some(sid) if !sid.is_empty() => by_session.entry(sid.clone()).or_default().push(r),
            _ => unassigned += 1,
        }
    }

    let mut parents_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut children_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (child, parent, _) in &edges {
        parents_of.entry(child.clone()).or_default().push(parent.clone());
        children_of.entry(parent.clone()).or_default().push(child.clone());
    }

    let mut summaries = Vec::new();
    for (sid, exs) in &by_session {
        let parents = parents_of.get(sid);
        // Only emit root sessions (or sessions whose parent is unobserved) as top-level trace entries.
        let has_observed_parent = parents.is_some_and(|ps| ps.iter().any(|p| by_session.contains_key(p)));
        if has_observed_parent {
            continue;
        }
        let mut visited = BTreeSet::new();
        let mut stack = vec![sid.clone()];
        let mut has_cycle = false;
        let mut has_conflict = false;
        let mut has_unobserved_parent = parents.is_some_and(|ps| ps.iter().any(|p| !by_session.contains_key(p)));
        let mut subtotal_cost = 0.0;
        let mut subtotal_tokens = 0u64;
        let mut total_ex = 0u64;
        let mut total_fail = 0u64;
        let mut started = exs.iter().map(|e| e.occurred_at).min().unwrap_or_else(Utc::now);
        let mut last = exs.iter().map(|e| e.occurred_at).max().unwrap_or_else(Utc::now);

        while let Some(curr) = stack.pop() {
            if !visited.insert(curr.clone()) {
                has_cycle = true;
                continue;
            }
            if let Some(ps) = parents_of.get(&curr) {
                if ps.len() > 1 {
                    has_conflict = true;
                }
                if ps.iter().any(|p| !by_session.contains_key(p)) {
                    has_unobserved_parent = true;
                }
            }
            if let Some(node_exs) = by_session.get(&curr) {
                for e in node_exs {
                    total_ex += 1;
                    if !is_success_outcome(&e.outcome) {
                        total_fail += 1;
                    }
                    subtotal_cost += e.api_equivalent_cost_usd.unwrap_or(0.0);
                    subtotal_tokens += (e.input_tokens.max(0) + e.output_tokens.max(0)) as u64;
                    started = started.min(e.occurred_at);
                    last = last.max(e.occurred_at);
                }
            }
            if let Some(kids) = children_of.get(&curr) {
                for k in kids {
                    stack.push(k.clone());
                }
            }
        }

        let own_cost: f64 = exs.iter().filter_map(|e| e.api_equivalent_cost_usd).sum();
        let own_tokens: u64 = exs.iter().map(|e| (e.input_tokens.max(0) + e.output_tokens.max(0)) as u64).sum();
        let head = exs[0];
        summaries.push(SessionTraceSummary {
            session_id: sid.clone(),
            root_session_id: sid.clone(),
            client: head.client.clone(),
            provider_id: head.provider_id.clone(),
            primary_model: head.effective_model.clone(),
            effort: head.effort.clone(),
            node_count: visited.len() as u64,
            exchange_count: total_ex,
            failure_count: total_fail,
            own_cost_usd: own_cost,
            subtotal_cost_usd: subtotal_cost,
            own_tokens,
            subtotal_tokens,
            has_unobserved_parent,
            has_parent_conflict: has_conflict,
            has_cycle,
            started_at: started,
            last_activity_at: last,
        });
    }

    summaries.sort_by_key(|a| std::cmp::Reverse(a.last_activity_at));
    let limit = scope.limit.unwrap_or(100) as usize;
    summaries.truncate(limit);
    Ok(TracePage { items: summaries, unassigned_exchanges: unassigned, next_cursor: None })
}

pub async fn trace_detail(hub: &Hub, actor: &Actor, session_id: &str) -> DResult<TraceDetail> {
    actor.require(ANY_READER)?;
    let all_edges: Vec<(String, String, String)> =
        sqlx::query_as("select child_session_id, parent_session_id, edge_kind from activity_session_edges").fetch_all(&hub.db).await?;

    let mut children_of: BTreeMap<String, Vec<(String, TraceEdgeKind)>> = BTreeMap::new();
    let mut parents_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (c, p, k) in &all_edges {
        children_of.entry(p.clone()).or_default().push((c.clone(), parse_enum(k, TraceEdgeKind::Subagent)));
        parents_of.entry(c.clone()).or_default().push(p.clone());
    }

    // BFS from session_id with cycle detection.
    let mut order: Vec<(String, Option<String>, Option<TraceEdgeKind>, u32)> = Vec::new();
    let mut visited = BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    let mut trace_edges = Vec::new();
    let mut has_cycle = false;
    queue.push_back((session_id.to_string(), None, None, 0u32));

    while let Some((sid, parent, kind, depth)) = queue.pop_front() {
        if !visited.insert(sid.clone()) {
            has_cycle = true;
            continue;
        }
        order.push((sid.clone(), parent, kind, depth));
        if let Some(kids) = children_of.get(&sid) {
            for (kid, ek) in kids {
                let conflict = parents_of.get(kid).is_some_and(|ps| ps.len() > 1);
                let cycle = visited.contains(kid);
                trace_edges.push(TraceEdge {
                    child_session_id: kid.clone(),
                    parent_session_id: sid.clone(),
                    kind: *ek,
                    observed_parent: true,
                    conflict,
                    cycle,
                });
                if !cycle {
                    queue.push_back((kid.clone(), Some(sid.clone()), Some(*ek), depth + 1));
                } else {
                    has_cycle = true;
                }
            }
        }
    }

    let sids: Vec<String> = visited.iter().cloned().collect();
    let rows = sqlx::query_as::<_, ExchangeRow>(
        "select e.id, e.source_exchange_id, e.source, e.purpose, e.client, e.client_version, e.transport,
                e.session_id, e.parent_session_id, e.parent_edge_kind, e.provider_id, e.account_id,
                e.moved_from_account_id, e.requested_model, e.effective_model, e.effort, e.outcome,
                e.status_code, e.error_message, e.latency_ms, e.ttft_ms, e.input_tokens, e.output_tokens,
                e.cache_read_tokens, e.cache_write_tokens, e.thinking_tokens, e.fast_mode,
                e.api_equivalent_cost_usd, e.capture_state, e.occurred_at,
                (select count(*) from activity_attempts a where a.exchange_id = e.id) as attempt_count
         from activity_exchanges e
         where e.session_id = any($1)
         order by e.occurred_at asc",
    )
    .bind(&sids)
    .fetch_all(&hub.db)
    .await?;

    if rows.is_empty() {
        return Err(DomainError::NotFound("session trace"));
    }

    let mut by_sid: BTreeMap<String, Vec<ExchangeRow>> = BTreeMap::new();
    for r in rows {
        if let Some(sid) = r.session_id.clone() {
            by_sid.entry(sid).or_default().push(r);
        }
    }

    let mut nodes = Vec::new();
    let mut subtotal_cost = 0.0;
    let mut subtotal_tokens = 0u64;
    let mut total_ex = 0u64;
    let mut total_fail = 0u64;
    let mut started = Utc::now();
    let mut last = DateTime::<Utc>::UNIX_EPOCH;
    let mut first_row: Option<RequestRow> = None;

    for (sid, parent, kind, depth) in &order {
        let exs = by_sid.get(sid).map(Vec::as_slice).unwrap_or(&[]);
        let own_requests = exs.len() as u64;
        let own_failures = exs.iter().filter(|e| !is_success_outcome(&e.outcome)).count() as u64;
        let own_tokens: u64 = exs.iter().map(|e| (e.input_tokens.max(0) + e.output_tokens.max(0)) as u64).sum();
        let own_cost: f64 = exs.iter().filter_map(|e| e.api_equivalent_cost_usd).sum();
        let unpriced = exs.iter().filter(|e| e.api_equivalent_cost_usd.is_none()).count() as u64;
        let accounts_used: Vec<String> = exs.iter().filter_map(|e| e.account_id.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        let moves: Vec<(String, String)> =
            exs.iter().filter_map(|e| Some((e.moved_from_account_id.clone()?, e.account_id.clone()?))).collect();
        let contract_exs: Vec<RequestRow> = exs.iter().map(ExchangeRow::to_contract).collect();
        for c in &contract_exs {
            if first_row.is_none() {
                first_row = Some(c.clone());
            }
            started = started.min(c.occurred_at);
            last = last.max(c.occurred_at);
        }
        subtotal_cost += own_cost;
        subtotal_tokens += own_tokens;
        total_ex += own_requests;
        total_fail += own_failures;
        nodes.push(TraceNode {
            session_id: sid.clone(),
            parent_session_id: parent.clone(),
            edge_kind: *kind,
            depth: *depth,
            own_requests,
            own_failures,
            own_tokens,
            own_cost_usd: own_cost,
            known_subtotal_cost_usd: own_cost,
            unpriced_requests: unpriced,
            accounts_used,
            moves,
            exchanges: contract_exs,
        });
    }

    let own_cost_by_sid: BTreeMap<String, f64> = nodes.iter().map(|n| (n.session_id.clone(), n.own_cost_usd)).collect();
    for node in &mut nodes {
        let mut seen = BTreeSet::new();
        let mut stack = vec![node.session_id.as_str()];
        let mut node_subtotal = 0.0;
        while let Some(curr) = stack.pop() {
            if !seen.insert(curr) {
                continue;
            }
            node_subtotal += own_cost_by_sid.get(curr).copied().unwrap_or(0.0);
            if let Some(kids) = children_of.get(curr) {
                for (kid, _) in kids {
                    stack.push(kid.as_str());
                }
            }
        }
        node.known_subtotal_cost_usd = node_subtotal;
    }

    let head = first_row.ok_or(DomainError::NotFound("session trace"))?;
    let own_cost_root = nodes.first().map(|n| n.own_cost_usd).unwrap_or(0.0);
    let own_tok_root = nodes.first().map(|n| n.own_tokens).unwrap_or(0);
    let summary = SessionTraceSummary {
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        client: head.client,
        provider_id: head.provider_id,
        primary_model: head.effective_model,
        effort: head.effort,
        node_count: nodes.len() as u64,
        exchange_count: total_ex,
        failure_count: total_fail,
        own_cost_usd: own_cost_root,
        subtotal_cost_usd: subtotal_cost,
        own_tokens: own_tok_root,
        subtotal_tokens,
        has_unobserved_parent: parents_of.get(session_id).is_some_and(|ps| !ps.is_empty()),
        has_parent_conflict: trace_edges.iter().any(|e| e.conflict),
        has_cycle,
        started_at: started,
        last_activity_at: last,
    };
    Ok(TraceDetail { summary, nodes, edges: trace_edges })
}

// ── 5. Drift & Canaries ─────────────────────────────────────────────────────

pub async fn drift(hub: &Hub, actor: &Actor, scope: ActivityScope) -> DResult<DriftReport> {
    actor.require(ANY_READER)?;
    let rows = query_exchanges(&hub.db, &scope, true, None).await?;

    let mut by_me_day: BTreeMap<(String, Option<String>, String), Vec<&ExchangeRow>> = BTreeMap::new();
    let mut version_seen: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for r in &rows {
        let date = r.occurred_at.format("%Y-%m-%d").to_string();
        by_me_day.entry((r.effective_model.clone(), r.effort.clone(), date.clone())).or_default().push(r);
        if let Some(ver) = &r.client_version {
            version_seen.entry((r.client.clone(), date)).or_default().insert(ver.clone());
        }
    }

    let mut grouped: BTreeMap<(String, Option<String>), Vec<DriftDayPoint>> = BTreeMap::new();
    for ((model, effort, date), day_rows) in by_me_day {
        let replies = day_rows.len() as u64;
        let thinking_sum: f64 = day_rows.iter().map(|r| r.thinking_tokens.max(0) as f64).sum();
        let output_sum: f64 = day_rows.iter().map(|r| r.output_tokens.max(0) as f64).sum();
        let no_think = day_rows.iter().filter(|r| r.thinking_tokens <= 0).count() as f64;
        let fails = day_rows.iter().filter(|r| !is_success_outcome(&r.outcome)).count() as f64;
        let mut ttfts: Vec<f64> = day_rows.iter().filter_map(|r| r.ttft_ms.map(|t| t as f64)).collect();
        ttfts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        grouped.entry((model, effort)).or_default().push(DriftDayPoint {
            date,
            replies,
            thinking_per_reply: if replies > 0 { thinking_sum / replies as f64 } else { 0.0 },
            output_per_reply: if replies > 0 { output_sum / replies as f64 } else { 0.0 },
            no_thinking_share: if replies > 0 { no_think / replies as f64 } else { 0.0 },
            median_ttft_ms: percentile_f64(&ttfts, 0.5),
            failure_rate: if replies > 0 { fails / replies as f64 } else { 0.0 },
            baseline_median_thinking: None,
            baseline_low: None,
            baseline_high: None,
            judgeable: false,
            outlier: false,
        });
    }

    let mut series = Vec::new();
    for ((model, effort), mut days) in grouped {
        days.sort_by(|a, b| a.date.cmp(&b.date));
        let mut outlier_days = 0u64;
        for i in 0..days.len() {
            let start = i.saturating_sub(14);
            let prior: Vec<f64> = days[start..i].iter().filter(|d| d.replies >= 30).map(|d| d.thinking_per_reply).collect();
            let judgeable = days[i].replies >= 30 && prior.len() >= 5;
            days[i].judgeable = judgeable;
            if judgeable {
                let mut sorted = prior;
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let med = percentile_f64(&sorted, 0.5).unwrap_or(0.0);
                let low = percentile_f64(&sorted, 0.15).unwrap_or(med * 0.7);
                let high = percentile_f64(&sorted, 0.85).unwrap_or(med * 1.3);
                days[i].baseline_median_thinking = Some(med);
                days[i].baseline_low = Some(low);
                days[i].baseline_high = Some(high);
                let val = days[i].thinking_per_reply;
                if (med > 0.0) && (val < low * 0.8 || val > high * 1.2) {
                    days[i].outlier = true;
                    outlier_days += 1;
                }
            }
        }
        let summary = if outlier_days > 0 {
            format!("{outlier_days} outlier day(s) outside 14-day normal band")
        } else {
            "Within baseline (or below 30 replies/day judgment threshold)".into()
        };
        series.push(DriftModelSeries { model, effort, days, outlier_days, summary });
    }

    // Surface client version changes as confounders, not attribution.
    let mut markers = Vec::new();
    let mut last_ver: BTreeMap<String, String> = BTreeMap::new();
    for ((client, date), versions) in version_seen {
        let joined = versions.into_iter().collect::<Vec<_>>().join(", ");
        if let Some(prev) = last_ver.get(&client)
            && prev != &joined
        {
            markers.push(DriftMarker { date, kind: "client_version".into(), label: format!("{client}: {prev} → {joined}") });
        }
        last_ver.insert(client, joined);
    }

    let schedule_raw: Option<Value> =
        sqlx::query_scalar("select value from settings where key = 'provider_canary_schedule'").fetch_optional(&hub.db).await?;
    let schedule: CanarySchedule = schedule_raw.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();

    let canary_rows: Vec<Value> =
        sqlx::query_scalar("select payload from model_probe_runs where kind = 'canary' order by ran_at desc limit 50")
            .fetch_all(&hub.db)
            .await?;
    let recent_canaries: Vec<CanaryRun> = canary_rows.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect();

    Ok(DriftReport { series, markers, schedule, recent_canaries })
}

// ── 6. Totals (durable aggregates) ──────────────────────────────────────────

pub async fn totals(hub: &Hub, actor: &Actor, granularity: TotalsGranularity, scope: ActivityScope) -> DResult<TotalsReport> {
    actor.require(ANY_READER)?;
    let from = scope.from.unwrap_or_else(|| Utc::now() - Duration::days(90));
    let to = scope.to.unwrap_or_else(Utc::now);
    let source_filter: Option<String> = match scope.source {
        None | Some(ActivitySource::All) => None,
        Some(other) => Some(enum_str(other, "proxy")),
    };
    let trunc = match granularity {
        TotalsGranularity::Hour => "hour",
        TotalsGranularity::Day => "day",
        TotalsGranularity::Week => "week",
        TotalsGranularity::Month => "month",
    };

    type TotalsQueryRow = (DateTime<Utc>, i64, i64, i64, i64, i64, i64, i64, f64, i64);
    let rows: Vec<TotalsQueryRow> = sqlx::query_as(
        "select date_trunc($1, hour_bucket) as period,
                sum(requests)::bigint,
                sum(failures)::bigint,
                sum(input_tokens)::bigint,
                sum(output_tokens)::bigint,
                sum(cache_read_tokens)::bigint,
                sum(cache_write_tokens)::bigint,
                sum(thinking_tokens)::bigint,
                sum(api_equivalent_cost_usd)::double precision,
                sum(unpriced_requests)::bigint
         from activity_totals
         where hour_bucket >= $2 and hour_bucket <= $3
           and ($4::text is null or source = $4)
           and ($5::text is null or provider_id = $5)
           and ($6::text is null or account_id = $6)
           and ($7::text is null or client = $7)
           and ($8::text is null or effective_model ilike '%' || $8 || '%')
           and ($9::text is null or effort = $9)
         group by 1
         order by 1 desc
         limit 366",
    )
    .bind(trunc)
    .bind(from)
    .bind(to)
    .bind(&source_filter)
    .bind(&scope.provider_id)
    .bind(&scope.account_id)
    .bind(&scope.client)
    .bind(&scope.model)
    .bind(&scope.effort)
    .fetch_all(&hub.db)
    .await?;

    let fmt = match granularity {
        TotalsGranularity::Hour => "%Y-%m-%d %H:00",
        TotalsGranularity::Day | TotalsGranularity::Week => "%Y-%m-%d",
        TotalsGranularity::Month => "%Y-%m",
    };

    type GrandTotalRow = (i64, i64, i64, i64, i64, i64, i64, f64, i64);
    let (g_reqs, g_fails, g_inp, g_out, g_cr, g_cw, g_th, g_cost, g_unp): GrandTotalRow = sqlx::query_as(
        "select coalesce(sum(requests), 0)::bigint,
                coalesce(sum(failures), 0)::bigint,
                coalesce(sum(input_tokens), 0)::bigint,
                coalesce(sum(output_tokens), 0)::bigint,
                coalesce(sum(cache_read_tokens), 0)::bigint,
                coalesce(sum(cache_write_tokens), 0)::bigint,
                coalesce(sum(thinking_tokens), 0)::bigint,
                coalesce(sum(api_equivalent_cost_usd), 0)::double precision,
                coalesce(sum(unpriced_requests), 0)::bigint
         from activity_totals
         where hour_bucket >= $1 and hour_bucket <= $2
           and ($3::text is null or source = $3)
           and ($4::text is null or provider_id = $4)
           and ($5::text is null or account_id = $5)
           and ($6::text is null or client = $6)
           and ($7::text is null or effective_model ilike '%' || $7 || '%')
           and ($8::text is null or effort = $8)",
    )
    .bind(from)
    .bind(to)
    .bind(&source_filter)
    .bind(&scope.provider_id)
    .bind(&scope.account_id)
    .bind(&scope.client)
    .bind(&scope.model)
    .bind(&scope.effort)
    .fetch_one(&hub.db)
    .await?;

    let grand = TotalsBucket {
        period: "Total".into(),
        requests: g_reqs.max(0) as u64,
        failures: g_fails.max(0) as u64,
        input_tokens: g_inp.max(0) as u64,
        output_tokens: g_out.max(0) as u64,
        cache_read_tokens: g_cr.max(0) as u64,
        cache_write_tokens: g_cw.max(0) as u64,
        thinking_tokens: g_th.max(0) as u64,
        api_equivalent_cost_usd: g_cost,
        unpriced_requests: g_unp.max(0) as u64,
        includes_retained_aggregates: true,
    };

    let mut buckets = Vec::with_capacity(rows.len());
    for (p, reqs, fails, inp, out, cr, cw, th, cost, unp) in rows {
        buckets.push(TotalsBucket {
            period: p.format(fmt).to_string(),
            requests: reqs.max(0) as u64,
            failures: fails.max(0) as u64,
            input_tokens: inp.max(0) as u64,
            output_tokens: out.max(0) as u64,
            cache_read_tokens: cr.max(0) as u64,
            cache_write_tokens: cw.max(0) as u64,
            thinking_tokens: th.max(0) as u64,
            api_equivalent_cost_usd: cost,
            unpriced_requests: unp.max(0) as u64,
            includes_retained_aggregates: true,
        });
    }

    Ok(TotalsReport { granularity, buckets, grand_total: grand })
}

// ── 7. Thinking Analysis & Tests ────────────────────────────────────────────

pub async fn thinking(hub: &Hub, actor: &Actor, scope: ActivityScope) -> DResult<ThinkingReport> {
    actor.require(ANY_READER)?;
    let rows = query_exchanges(&hub.db, &scope, true, None).await?;
    let catalog = pricing::default_catalog();

    let mut total_turns = 0u64;
    let mut thinking_tokens = 0u64;
    let mut thinking_cost = 0.0;
    let mut total_cost = 0.0;
    let mut buckets_map: BTreeMap<&'static str, ThinkingBucket> = BTreeMap::new();
    let mut kinds_map: BTreeMap<String, ThinkingBucket> = BTreeMap::new();

    for r in &rows {
        total_turns += 1;
        let th = r.thinking_tokens.max(0) as u64;
        let out = r.output_tokens.max(0) as u64;
        thinking_tokens += th;
        let row_cost = r.api_equivalent_cost_usd.unwrap_or(0.0);
        total_cost += row_cost;
        let t_cost = pricing::lookup_price(&r.effective_model, &catalog).map(|p| (th as f64 * p.output_per_m) / 1_000_000.0).unwrap_or(0.0);
        thinking_cost += t_cost;

        let bucket_label = match th {
            0 => "0 (no thinking)",
            1..=500 => "1–500",
            501..=2000 => "501–2k",
            2001..=8000 => "2k–8k",
            _ => "8k+",
        };
        let b = buckets_map.entry(bucket_label).or_insert_with(|| ThinkingBucket {
            label: bucket_label.into(),
            turns: 0,
            thinking_tokens: 0,
            output_tokens: 0,
            estimated_thinking_cost_usd: 0.0,
        });
        b.turns += 1;
        b.thinking_tokens += th;
        b.output_tokens += out;
        b.estimated_thinking_cost_usd += t_cost;

        let kind_label = r.effort.clone().unwrap_or_else(|| "default".into());
        let k = kinds_map.entry(kind_label.clone()).or_insert_with(|| ThinkingBucket {
            label: kind_label,
            turns: 0,
            thinking_tokens: 0,
            output_tokens: 0,
            estimated_thinking_cost_usd: 0.0,
        });
        k.turns += 1;
        k.thinking_tokens += th;
        k.output_tokens += out;
        k.estimated_thinking_cost_usd += t_cost;
    }

    let alternatives = vec![
        ThinkingAlternative {
            label: "Lower effort (-45% thinking tokens est.)".into(),
            target_model: scope.model.clone().unwrap_or_else(|| "current model".into()),
            target_effort: Some("medium".into()),
            estimated_cost_usd: (total_cost - thinking_cost * 0.45).max(0.0),
            estimated_savings_usd: thinking_cost * 0.45,
            estimated_savings_pct: if total_cost > 0.0 { (thinking_cost * 0.45 / total_cost) * 100.0 } else { 0.0 },
            is_estimate: true,
        },
        ThinkingAlternative {
            label: "Cheaper tier at same effort (~65% list-rate est.)".into(),
            target_model: "mid-tier equivalent".into(),
            target_effort: scope.effort.clone(),
            estimated_cost_usd: total_cost * 0.35,
            estimated_savings_usd: total_cost * 0.65,
            estimated_savings_pct: if total_cost > 0.0 { 65.0 } else { 0.0 },
            is_estimate: true,
        },
    ];

    let test_rows: Vec<Value> =
        sqlx::query_scalar("select payload from model_probe_runs where kind = 'thinking_test' order by ran_at desc limit 30")
            .fetch_all(&hub.db)
            .await?;
    let test_runs: Vec<ThinkingTestRun> = test_rows.into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect();

    let share_pct = if total_cost > 0.0 { (thinking_cost / total_cost) * 100.0 } else { 0.0 };
    let verdict = if total_turns == 0 {
        "No turns observed in the selected scope.".into()
    } else if share_pct >= 40.0 {
        format!("Thinking accounts for ~{share_pct:.1}% of estimated API-equivalent cost — lower effort may yield material savings.")
    } else {
        format!("Thinking accounts for ~{share_pct:.1}% of estimated API-equivalent cost across {total_turns} turns.")
    };

    Ok(ThinkingReport {
        selected_model: scope.model,
        selected_effort: scope.effort,
        verdict,
        total_turns,
        thinking_tokens,
        thinking_cost_usd: thinking_cost,
        buckets: buckets_map.into_values().collect(),
        kinds: kinds_map.into_values().collect(),
        alternatives,
        method_note: "Cost breakdowns are list-price estimates; run Thinking Tests to empirically sample recorded turns without tools."
            .into(),
        test_runs,
        board_tiles: vec![
            BoardTile { id: "verdict".into(), kind: "verdict".into(), metric: "cost".into(), span: 3, visible: true },
            BoardTile { id: "buckets".into(), kind: "buckets".into(), metric: "thinking".into(), span: 1, visible: true },
            BoardTile { id: "kinds".into(), kind: "kinds".into(), metric: "thinking".into(), span: 1, visible: true },
            BoardTile { id: "alternatives".into(), kind: "alternatives".into(), metric: "savings".into(), span: 1, visible: true },
        ],
    })
}

// ── 8. Evals (pinned cases, predicates, replay & import/export) ─────────────

pub async fn list_evals(hub: &Hub, actor: &Actor) -> DResult<Vec<EvalCase>> {
    actor.require(ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select jsonb_build_object(
            'id', c.id,
            'name', c.name,
            'source_exchange_id', c.source_exchange_id,
            'provider_id', c.provider_id,
            'model_id', c.model_id,
            'effort', c.effort,
            'prompt_text', c.prompt_text,
            'expected_outcome', c.expected_outcome,
            'predicates', c.predicates,
            'last_replay', (
                select to_jsonb(r) from (
                    select id, case_id, model_id, effort, account_id, actual_outcome,
                           response_preview, passed, predicate_results, latency_ms, replayed_at
                    from model_eval_replays where case_id = c.id order by replayed_at desc limit 1
                ) r
            ),
            'created_at', c.created_at,
            'updated_at', c.updated_at
         ) from model_eval_cases c order by c.updated_at desc",
    )
    .fetch_all(&hub.db)
    .await?;
    crate::decode(rows)
}

pub async fn create_eval(hub: &Hub, actor: &Actor, input: EvalCaseIn) -> DResult<EvalCase> {
    actor.require(OPERATOR)?;
    if input.name.trim().is_empty() || input.prompt_text.trim().is_empty() {
        return Err(invalid("invalid_eval_case", "name and prompt_text are required"));
    }
    let id = Uuid::new_v4();
    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "insert into model_eval_cases
            (id, name, source_exchange_id, provider_id, model_id, effort, prompt_text, expected_outcome, predicates)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(input.name.trim())
    .bind(&input.source_exchange_id)
    .bind(&input.provider_id)
    .bind(&input.model_id)
    .bind(&input.effort)
    .bind(&input.prompt_text)
    .bind(&input.expected_outcome)
    .bind(serde_json::to_value(&input.predicates).unwrap_or_else(|_| json!([])))
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "providers.eval.created", "eval_case", id, json!({ "name": input.name })).await?;
    tx.commit().await?;
    get_eval(hub, actor, id).await
}

pub async fn update_eval(hub: &Hub, actor: &Actor, id: Uuid, input: EvalCaseIn) -> DResult<EvalCase> {
    actor.require(OPERATOR)?;
    let res = sqlx::query(
        "update model_eval_cases
         set name = $2, source_exchange_id = $3, provider_id = $4, model_id = $5, effort = $6,
             prompt_text = $7, expected_outcome = $8, predicates = $9, updated_at = now()
         where id = $1",
    )
    .bind(id)
    .bind(input.name.trim())
    .bind(&input.source_exchange_id)
    .bind(&input.provider_id)
    .bind(&input.model_id)
    .bind(&input.effort)
    .bind(&input.prompt_text)
    .bind(&input.expected_outcome)
    .bind(serde_json::to_value(&input.predicates).unwrap_or_else(|_| json!([])))
    .execute(&hub.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(DomainError::NotFound("eval case"));
    }
    get_eval(hub, actor, id).await
}

pub async fn delete_eval(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<()> {
    actor.require(OPERATOR)?;
    let res = sqlx::query("delete from model_eval_cases where id = $1").bind(id).execute(&hub.db).await?;
    if res.rows_affected() == 0 {
        return Err(DomainError::NotFound("eval case"));
    }
    Ok(())
}

async fn get_eval(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<EvalCase> {
    let all = list_evals(hub, actor).await?;
    all.into_iter().find(|c| c.id == id).ok_or(DomainError::NotFound("eval case"))
}

pub fn evaluate_predicates(predicates: &[EvalPredicate], output: &str) -> Vec<EvalPredicateResult> {
    predicates
        .iter()
        .map(|p| {
            let (hay, needle) = if p.case_sensitive {
                (output.to_string(), p.value.clone())
            } else {
                (output.to_ascii_lowercase(), p.value.to_ascii_lowercase())
            };
            let passed = match p.kind {
                EvalPredicateKind::Contains => hay.contains(&needle),
                EvalPredicateKind::NotContains => !hay.contains(&needle),
                EvalPredicateKind::Equals => hay.trim() == needle.trim(),
            };
            EvalPredicateResult { predicate: p.clone(), passed }
        })
        .collect()
}

/// Replay a pinned text-only eval case through its registered provider's probe facet.
pub async fn replay_eval(
    hub: &Hub,
    actor: &Actor,
    id: Uuid,
    account_id: Option<String>,
    model_override: Option<String>,
    effort_override: Option<String>,
) -> DResult<EvalReplayResult> {
    actor.require(OPERATOR)?;
    let case = get_eval(hub, actor, id).await?;
    let model_id = model_override.unwrap_or_else(|| case.model_id.clone());
    let effort = effort_override.or_else(|| case.effort.clone());

    // Submit through the generic provider operation layer if provider supports ReplayTextEval.
    let op = super::submit_operation(
        hub,
        actor,
        &case.provider_id,
        SubmitOperationIn {
            idempotency_key: format!("eval-replay-{}-{}", case.id, Uuid::new_v4()),
            expected_revision: None,
            target_account_id: account_id.clone(),
            target_host: None,
            target_user: None,
            command: ProviderCommand::Probe(ProbeCommand::ReplayTextEval {
                case_id: case.id,
                account_id: account_id.clone(),
                model_override: Some(model_id.clone()),
                effort_override: effort.clone(),
            }),
        },
    )
    .await?;

    let response_preview = op.receipt.as_ref().and_then(|r| r.get("response_text").and_then(|v| v.as_str())).unwrap_or("").to_string();
    let actual_outcome = if op.status == OperationStatus::Succeeded { "ok".to_string() } else { "failed".to_string() };
    let predicate_results = evaluate_predicates(&case.predicates, &response_preview);
    let passed = (actual_outcome == case.expected_outcome
        || (is_success_outcome(&actual_outcome) && is_success_outcome(&case.expected_outcome)))
        && predicate_results.iter().all(|r| r.passed);
    let latency_ms = op.receipt.as_ref().and_then(|r| r.get("latency_ms").and_then(|v| v.as_i64()));

    let replay_id = Uuid::new_v4();
    let now = Utc::now();
    sqlx::query(
        "insert into model_eval_replays
            (id, case_id, model_id, effort, account_id, actual_outcome, response_preview, passed, predicate_results, latency_ms, replayed_at)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(replay_id)
    .bind(case.id)
    .bind(&model_id)
    .bind(&effort)
    .bind(&account_id)
    .bind(&actual_outcome)
    .bind(&response_preview)
    .bind(passed)
    .bind(serde_json::to_value(&predicate_results).unwrap_or_else(|_| json!([])))
    .bind(latency_ms)
    .bind(now)
    .execute(&hub.db)
    .await?;

    Ok(EvalReplayResult {
        id: replay_id,
        case_id: case.id,
        model_id,
        effort,
        account_id,
        actual_outcome,
        response_preview,
        passed,
        predicate_results,
        latency_ms,
        replayed_at: now,
    })
}

pub async fn export_evals(hub: &Hub, actor: &Actor, include_bodies: bool) -> DResult<EvalExportBundle> {
    actor.require(OPERATOR)?;
    let mut cases = list_evals(hub, actor).await?;
    if !include_bodies {
        for c in &mut cases {
            c.prompt_text = "<omitted>".into();
            if let Some(replay) = &mut c.last_replay {
                replay.response_preview = "<omitted>".into();
            }
        }
    }
    Ok(EvalExportBundle { schema_version: 1, exported_at: Utc::now(), include_bodies, cases })
}

pub async fn import_evals(hub: &Hub, actor: &Actor, bundle: EvalExportBundle) -> DResult<Vec<EvalCase>> {
    actor.require(OPERATOR)?;
    let mut created = Vec::new();
    for c in bundle.cases {
        let item = create_eval(
            hub,
            actor,
            EvalCaseIn {
                name: c.name,
                source_exchange_id: c.source_exchange_id,
                provider_id: c.provider_id,
                model_id: c.model_id,
                effort: c.effort,
                prompt_text: c.prompt_text,
                expected_outcome: c.expected_outcome,
                predicates: c.predicates,
            },
        )
        .await?;
        created.push(item);
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_predicates_handle_contains_not_contains_and_equals() {
        let preds = vec![
            EvalPredicate { kind: EvalPredicateKind::Contains, value: "rust".into(), case_sensitive: false },
            EvalPredicate { kind: EvalPredicateKind::NotContains, value: "error".into(), case_sensitive: false },
            EvalPredicate { kind: EvalPredicateKind::Equals, value: "Rust works".into(), case_sensitive: true },
        ];
        let res = evaluate_predicates(&preds, "  Rust works  ");
        assert!(res.iter().all(|r| r.passed));
        let fail = evaluate_predicates(&preds, "rust error");
        assert!(fail[0].passed);
        assert!(!fail[1].passed);
        assert!(!fail[2].passed);
    }

    #[test]
    fn success_outcomes_include_proxy_success_variant() {
        assert!(is_success_outcome("ok"));
        assert!(is_success_outcome("succeeded"));
        assert!(is_success_outcome("success"));
        assert!(!is_success_outcome("failed"));
        assert!(!is_success_outcome("rate_limited"));
    }

    #[test]
    fn cursor_roundtrips_timestamp_and_uuid_and_supports_legacy_timestamp() {
        let ts = DateTime::parse_from_rfc3339("2026-11-21T12:34:56Z").unwrap().with_timezone(&Utc);
        let id = Uuid::new_v4();
        let encoded = format_cursor(ts, id);
        let (parsed_ts, parsed_id) = parse_cursor(&encoded).unwrap();
        assert_eq!(parsed_ts, ts);
        assert_eq!(parsed_id, Some(id));

        let (legacy_ts, legacy_id) = parse_cursor("2026-11-21T12:34:56Z").unwrap();
        assert_eq!(legacy_ts, ts);
        assert_eq!(legacy_id, None);
        assert!(parse_cursor("not-a-timestamp").is_err());
    }
}
