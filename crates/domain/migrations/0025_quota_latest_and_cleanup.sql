-- Issue 2 cleanup: remove leaked fixture accounts/providers and stale legacy pre-0017 accounts
-- before backfilling `model_quota_latest`.
delete from model_accounts
where host = 'fixture-host'
  and source_publisher is null
  and publisher_host is null
  and provider_id in (select id from model_provider_instances where mode = 'live');

delete from model_provider_instances
where mode = 'fixture'
  and exists (select 1 from model_provider_instances where mode = 'live');

delete from model_accounts
where source_publisher is null
  and (source_account_id like 'proxy:%' or id like '%:proxy:%');

delete from model_accounts
where source_publisher is null
  and source_account_id like 'local-%'
  and last_observed_at < now() - interval '12 hours';

-- Issue 1: materialized latest quota window snapshot per (provider, account, window, scope_model)
-- and supporting indexes on `model_quota_samples`.
create table model_quota_latest (
    sample_id bigint not null default 0,
    provider_id text not null references model_provider_instances(id) on delete cascade,
    account_id text not null,
    window_key text not null,
    scope_model_key text not null default '',
    label text not null,
    kind text not null default 'custom',
    unit text not null default 'percent',
    used_percent double precision,
    remaining_percent double precision,
    used_amount double precision,
    limit_amount double precision,
    currency text,
    resets_at timestamptz,
    window_duration_mins int,
    cadence text not null default 'unknown',
    scope_model text,
    status text not null default 'unknown',
    observation_state text not null default 'never_observed',
    observed_at timestamptz not null,
    recorded_at timestamptz not null default now(),
    stale boolean not null default false,
    error_reason text,
    primary key (provider_id, account_id, window_key, scope_model_key)
);

create index if not exists model_quota_samples_latest_expr_idx
    on model_quota_samples (provider_id, account_id, window_key, coalesce(scope_model, ''), observed_at desc, id desc);

create index if not exists model_quota_samples_observed_at_idx
    on model_quota_samples (observed_at);

-- Backfill `model_quota_latest` only for accounts still present in `model_accounts`,
-- excluding stale abandoned windows (>1 hour older than the account's latest window)
-- and `scope_model IS NULL` rows superseded by a newer or equal `scope_model IS NOT NULL` row.
with latest as (
    select distinct on (s.provider_id, s.account_id, s.window_key, coalesce(s.scope_model, ''))
        s.id as sample_id,
        s.provider_id,
        s.account_id,
        s.window_key,
        coalesce(s.scope_model, '') as scope_model_key,
        s.label,
        s.kind,
        s.unit,
        s.used_percent,
        s.remaining_percent,
        s.used_amount,
        s.limit_amount,
        s.currency,
        s.resets_at,
        s.window_duration_mins,
        s.cadence,
        nullif(s.scope_model, '') as scope_model,
        s.status,
        s.observation_state,
        s.observed_at,
        s.recorded_at,
        s.stale,
        s.error_reason
    from model_quota_samples s
    join model_accounts ma
      on ma.provider_id = s.provider_id
     and ma.id = s.account_id
    order by s.provider_id, s.account_id, s.window_key, coalesce(s.scope_model, ''), s.observed_at desc, s.id desc
),
with_acct_max as (
    select
        l.*,
        max(l.observed_at) over (partition by l.provider_id, l.account_id) as acct_max_observed_at
    from latest l
),
filtered as (
    select w.*
    from with_acct_max w
    where w.acct_max_observed_at - w.observed_at <= interval '1 hour'
      and not (
          w.scope_model is null
          and exists (
              select 1
              from with_acct_max scoped
              where scoped.provider_id = w.provider_id
                and scoped.account_id = w.account_id
                and scoped.window_key = w.window_key
                and scoped.scope_model is not null
                and scoped.observed_at >= w.observed_at
          )
      )
)
insert into model_quota_latest (
    sample_id, provider_id, account_id, window_key, scope_model_key,
    label, kind, unit, used_percent, remaining_percent,
    used_amount, limit_amount, currency, resets_at, window_duration_mins,
    cadence, scope_model, status, observation_state, observed_at,
    recorded_at, stale, error_reason
)
select
    sample_id, provider_id, account_id, window_key, scope_model_key,
    label, kind, unit, used_percent, remaining_percent,
    used_amount, limit_amount, currency, resets_at, window_duration_mins,
    cadence, scope_model, status, observation_state, observed_at,
    recorded_at, stale, error_reason
from filtered
on conflict (provider_id, account_id, window_key, scope_model_key) do nothing;
