use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use contracts::providers::{QuotaWindow, infer_window_duration_mins, window_scope_family};
use serde_json::Value;
use sqlx::PgPool;

use crate::DResult;

pub(super) async fn compute_observed_capacity_ratios(
    db: &PgPool,
    provider_id: &str,
    as_of: DateTime<Utc>,
    exclude_fixture_host: bool,
) -> DResult<BTreeMap<String, f64>> {
    // Split each stream on reset changes or falling usage. Measuring the entire
    // monotonic span also captures burn when no individual step reaches 2%.
    let rows: Vec<(String, Option<f64>, Value, f64, f64)> = sqlx::query_as(
        "with measured as (
            select s.*, lower(scope_model) as scope, coalesce(used_percent, 100.0 - remaining_percent) as used
            from model_quota_samples s
            where s.provider_id = $1 and s.observed_at <= $2 and s.unit = 'percent'
              and s.observation_state = 'measured'
         ), boundaries as (
            select *, case when resets_at is distinct from lag(resets_at) over stream
                              or used < lag(used) over stream then 1 else 0 end as boundary
            from measured where used between 0 and 100
            window stream as (partition by account_id, window_key, scope order by observed_at, id)
         ), segments as (
            select *, sum(boundary) over (
                partition by account_id, window_key, scope order by observed_at, id
            ) as segment
            from boundaries
         ), spans as (
            select account_id, window_key, scope, segment,
                   max(kind) as kind, max(window_duration_mins) as duration, min(observed_at) as start_at,
                   max(observed_at) as end_at, max(used) - min(used) as burn
            from segments where observed_at < resets_at
            group by account_id, window_key, scope, segment
            having max(used) - min(used) >= 2.0 and max(observed_at) > min(observed_at)
         )
         select a.id, a.tier_multiplier,
                jsonb_build_object('window_key', s.window_key, 'label', s.window_key, 'kind', s.kind,
                                  'window_duration_mins', s.duration, 'scope_model', s.scope),
                sum(t.tokens)::double precision, sum(s.burn)
         from spans s
         join model_accounts a on a.provider_id = $1 and a.id = s.account_id
         cross join lateral (
            select sum(input_tokens::double precision + output_tokens) as tokens
            from activity_exchanges
            where provider_id = $1 and account_id = s.account_id
              and occurred_at > s.start_at and occurred_at <= s.end_at
              and (s.scope is null or lower(effective_model) = s.scope
                   or lower(effective_model) like any (case
                       when s.scope like '%gemini%' then array['%gemini%']
                       when s.scope like any (array['%claude & gpt%', '%3p%'])
                           then array['%claude%', '%gpt%', '%o1%', '%o3%', '%o4%', '%codex%']
                       when s.scope like '%opus%' then array['%opus%']
                       when s.scope like '%sonnet%' then array['%sonnet%']
                       when s.scope like '%haiku%' then array['%haiku%']
                       when s.scope like '%claude%' and s.scope not like '%& gpt%' then array['%claude%']
                       when s.scope like any (array['%codex%', '%gpt%']) then array['%gpt%', '%codex%']
                   end))
         ) t
         where t.tokens > 0
           and (not $3 or not (a.host = 'fixture-host' and a.source_publisher is null and a.publisher_host is null))
         group by a.id, a.tier_multiplier, s.window_key, s.scope, s.kind, s.duration
         order by a.id, s.window_key, s.scope, s.kind, s.duration",
    )
    .bind(provider_id)
    .bind(as_of)
    .bind(exclude_fixture_host)
    .fetch_all(db)
    .await?;

    // Compare only matching streams. Within a cohort, use the largest burn
    // per account rather than counting overlapping quota observations twice.
    let mut cohorts = BTreeMap::<_, BTreeMap<_, _>>::new();
    for (account_id, multiplier, window, tokens, burn) in rows {
        let window: QuotaWindow = crate::decode_one(window)?;
        let Some(horizon) = infer_window_duration_mins(&window) else {
            continue;
        };
        let implied = tokens / (burn / 100.0);
        if implied.is_finite() && implied > 0.0 {
            let cohort = (horizon, window_scope_family(&window.window_key), window.scope_model);
            let entry = cohorts.entry(cohort).or_default().entry(account_id).or_insert((burn, multiplier, implied));
            if burn > entry.0 {
                *entry = (burn, multiplier, implied);
            }
        }
    }
    // Normalize within each cohort; visit the longest first so each account keeps its best comparison.
    let mut ratios = BTreeMap::new();
    for observed in cohorts.into_values().rev().filter(|accounts| accounts.len() >= 2) {
        let mut baselines = observed
            .values()
            .filter_map(|&(_, multiplier, implied)| multiplier.is_none_or(|m| (0.9..=1.1).contains(&m)).then_some(implied))
            .collect::<Vec<_>>();
        if baselines.is_empty() {
            baselines.extend(
                observed
                    .values()
                    .filter_map(|&(_, multiplier, implied)| multiplier.filter(|m| m.is_finite() && *m > 0.0).map(|m| implied / m)),
            );
        }
        baselines.sort_by(f64::total_cmp);
        let middle = baselines.len() / 2;
        let Some(&upper) = baselines.get(middle) else { continue };
        let baseline = if baselines.len() % 2 == 0 { upper.midpoint(baselines[middle - 1]) } else { upper };
        if !baseline.is_finite() || baseline <= 0.0 {
            continue;
        }
        for (id, (_, _, implied)) in observed {
            let ratio = implied / baseline;
            if ratio.is_finite() {
                ratios.entry(id).or_insert(ratio);
            }
        }
    }
    Ok(ratios)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn observed_ratios(scoped: bool) -> Option<BTreeMap<String, f64>> {
        let Ok(url) = std::env::var("HUB_TEST_DATABASE_URL") else {
            eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
            return None;
        };
        let db = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
        let at = DateTime::parse_from_rfc3339("2026-11-21T12:00:00Z").unwrap().to_utc();
        sqlx::raw_sql(
            "create temporary table model_accounts (
                id text, provider_id text, tier_multiplier double precision, host text, source_publisher text, publisher_host text
            );
            create temporary table model_quota_samples (
                id bigint generated always as identity, provider_id text, account_id text, window_key text, kind text,
                scope_model text, window_duration_mins int, unit text, observation_state text, used_percent double precision,
                remaining_percent double precision, observed_at timestamptz, resets_at timestamptz
            );
            create temporary table activity_exchanges (
                provider_id text, account_id text, effective_model text, input_tokens bigint, output_tokens bigint, occurred_at timestamptz
            );
            insert into model_accounts (id, provider_id, tier_multiplier)
            values ('weekly-base', 'test', 1), ('weekly-max', 'test', 2), ('short-base', 'test', 1), ('short-max', 'test', 3);",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "insert into model_quota_samples
                (provider_id, account_id, window_key, kind, scope_model, unit, observation_state, used_percent, observed_at, resets_at)
             select 'test', id, window_key, kind, case when $2 then scope end, 'percent', 'measured',
                    used + case when id = 'weekly-max' and kind = 'session' and used = 15 then 5 else 0 end,
                    $1::timestamptz + elapsed, $1 + interval '1 hour'
             from (values ('weekly-base', 'weekly', 'weekly', 'Gemini'),
                          ('weekly-max', 'weekly', 'weekly', 'Gemini'),
                          ('weekly-max', '5h', 'session', 'Claude & GPT (shared)'),
                          ('short-base', '5h', 'session', 'Claude & GPT (shared)'),
                          ('short-max', '5h', 'session', 'Claude & GPT (shared)')) as cohort(id, window_key, kind, scope)
             cross join (values (10, interval '-1 minute'), (15, interval '0')) as sample(used, elapsed)",
        )
        .bind(at)
        .bind(scoped)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "insert into activity_exchanges
             select 'test', id, model, tokens, 0, $1 from (values
                ('weekly-base', 'gemini-3.8-flash', 1000), ('weekly-max', 'gemini-3.8-flash', 2000),
                ('short-base', 'claude-opus-5-5', 100), ('short-max', 'o3-mini', 300),
                ('weekly-base', 'claude-opus-5-5', 9000), ('short-base', 'gemini-3.8-flash', 900)
             ) as exchange(id, model, tokens) where $2 or tokens not in (9000, 900)",
        )
        .bind(at)
        .bind(scoped)
        .execute(&db)
        .await
        .unwrap();
        Some(compute_observed_capacity_ratios(&db, "test", at, false).await.unwrap())
    }

    #[tokio::test]
    async fn model_scopes_match_production_buckets() {
        if let Some(ratios) = observed_ratios(true).await {
            assert_eq!(ratios.get("weekly-max"), Some(&2.0));
            assert_eq!(ratios.get("short-max"), Some(&3.0));
        }
    }

    #[tokio::test]
    async fn independently_normalizes_weekly_and_short_cohorts() {
        if let Some(ratios) = observed_ratios(false).await {
            assert_eq!(
                ratios,
                BTreeMap::from([
                    ("weekly-base".into(), 1.0),
                    ("weekly-max".into(), 2.0),
                    ("short-base".into(), 1.0),
                    ("short-max".into(), 3.0)
                ])
            );
        }
    }
}
