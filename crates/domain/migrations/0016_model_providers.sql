-- Generic AI model providers, multi-account inventory, quota sampling,
-- operations, activity/traces/drift/thinking/evals, routing and capture policy.

alter table principals drop constraint if exists principals_role_check;
alter table principals add constraint principals_role_check
    check (role in ('operator', 'agent', 'procedure', 'ingest', 'member', 'collector'));

-- Ensure one stable row per principal name so token rotation preserves IDs.
create unique index if not exists principals_name_unique on principals (name);

-- Registered or observed model provider instances (e.g. antigravity, claude, opencode, proxy).
create table model_provider_instances (
    id text primary key,
    label text not null,
    family text not null,
    mode text not null default 'live',
    verification text not null default 'built',
    router_id text,
    health text not null default 'unobserved',
    status_message text,
    capabilities jsonb not null default '[]',
    evidence_kinds jsonb not null default '[]',
    last_observed_at timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

-- Scoped source grants binding collector/principal names to allowed hosts, users,
-- providers and evidence kinds.
create table model_source_grants (
    id uuid primary key,
    principal_name text not null,
    host text not null,
    unix_user text not null,
    provider_ids jsonb not null default '[]',
    evidence_kinds jsonb not null default '[]',
    can_claim_operations boolean not null default false,
    created_at timestamptz not null default now(),
    unique (principal_name, host, unix_user)
);

-- Logical multi-account inventory per model provider.
create table model_accounts (
    id text not null,
    provider_id text not null references model_provider_instances(id) on delete cascade,
    source_account_id text not null,
    display_name text not null,
    email text,
    masked_email text,
    note text,
    plan text,
    tier_multiplier double precision,
    enabled boolean not null default true,
    priority int not null default 0,
    weight int not null default 1,
    cooldown_until timestamptz,
    status_message text,
    is_active boolean not null default false,
    is_preferred boolean not null default false,
    is_next_pick boolean not null default false,
    enrollment_state text not null default 'unobserved',
    host text,
    unix_user text,
    credential_fingerprint text,
    last_used_at timestamptz,
    last_observed_at timestamptz,
    capabilities jsonb not null default '[]',
    success_count bigint not null default 0,
    failure_count bigint not null default 0,
    rolling_starter jsonb,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    primary key (provider_id, id)
);
create index model_accounts_provider_idx on model_accounts (provider_id, priority desc, id);

-- Immutable quota window observations for current state, history charts,
-- past-window summaries, and drain-rate forecasting.
create table model_quota_samples (
    id bigserial primary key,
    provider_id text not null references model_provider_instances(id) on delete cascade,
    account_id text not null,
    window_key text not null,
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
    error_reason text
);
create index model_quota_samples_lookup_idx
    on model_quota_samples (provider_id, account_id, window_key, observed_at desc);
create index model_quota_samples_time_idx
    on model_quota_samples (provider_id, observed_at desc);

-- Provider reset credits / grants per account.
create table model_reset_grants (
    provider_id text not null references model_provider_instances(id) on delete cascade,
    account_id text not null,
    grant_id text not null,
    available_count int not null default 0,
    applicable boolean not null default false,
    checked_at timestamptz not null,
    expires_at timestamptz,
    scope text,
    cleared_windows jsonb not null default '[]',
    weekly_credit_back boolean not null default false,
    unavailable_reason text,
    updated_at timestamptz not null default now(),
    primary key (provider_id, account_id)
);

-- API key upstream groups (masked keys only; raw secrets stay with owner).
create table model_key_groups (
    id text not null,
    provider_id text not null references model_provider_instances(id) on delete cascade,
    family text not null,
    name text not null,
    base_url text,
    enabled boolean not null default true,
    request_identity text not null default 'api',
    keys jsonb not null default '[]',
    configured_models jsonb not null default '[]',
    discovered_models jsonb not null default '[]',
    aliases jsonb not null default '[]',
    updated_at timestamptz not null default now(),
    primary key (provider_id, id)
);

-- Discovered model catalog entries.
create table model_catalog (
    provider_id text not null references model_provider_instances(id) on delete cascade,
    model_id text not null,
    display_name text,
    family text,
    group_id text,
    context_window bigint,
    supports_thinking boolean,
    discovered_at timestamptz,
    primary key (provider_id, model_id)
);

-- Effective-dated API list prices (explicitly distinct from subscription billing).
create table model_prices (
    model_pattern text not null,
    effective_from date not null,
    display_name text not null,
    currency text not null default 'USD',
    input_per_m double precision not null,
    output_per_m double precision not null,
    cache_read_per_m double precision not null,
    cache_write_5m_per_m double precision not null,
    source_note text not null default 'list_price',
    primary key (model_pattern, effective_from)
);

-- Routing & proxy runtime state per provider/router.
create table provider_routing_state (
    provider_id text primary key references model_provider_instances(id) on delete cascade,
    router_id text,
    revision bigint not null default 1,
    runtime jsonb,
    policy jsonb not null default '{}',
    clients jsonb not null default '[]',
    lockouts jsonb not null default '[]',
    recent_logs jsonb not null default '[]',
    rolling_starter jsonb not null default '{}',
    updated_at timestamptz not null default now()
);

-- Typed provider operation intents, idempotent dispatch, collector claim & receipt.
create table model_operations (
    id uuid primary key,
    provider_id text not null,
    actor text not null,
    idempotency_key text not null,
    request_sha256 text not null,
    command_kind text not null,
    command jsonb not null,
    target_account_id text,
    target_host text,
    target_user text,
    expected_revision bigint,
    status text not null default 'pending',
    claimed_by text,
    claimed_at timestamptz,
    receipt jsonb,
    error text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (provider_id, idempotency_key)
);
create index model_operations_claim_idx on model_operations (status, target_host, target_user, created_at);

-- Activity exchanges (one inbound client exchange, reconciled across sources).
create table activity_exchanges (
    id uuid primary key,
    source_host text not null,
    source_user text not null,
    source_exchange_id text not null,
    source text not null default 'proxy',
    purpose text not null default 'ordinary',
    client text not null,
    client_version text,
    transport text,
    session_id text,
    parent_session_id text,
    parent_edge_kind text,
    provider_id text not null,
    account_id text,
    moved_from_account_id text,
    requested_model text not null,
    effective_model text not null,
    effort text,
    outcome text not null,
    status_code int,
    error_message text,
    latency_ms bigint,
    ttft_ms bigint,
    input_tokens bigint not null default 0,
    output_tokens bigint not null default 0,
    cache_read_tokens bigint not null default 0,
    cache_write_tokens bigint not null default 0,
    thinking_tokens bigint not null default 0,
    fast_mode boolean not null default false,
    api_equivalent_cost_usd double precision,
    capture_state text not null default 'none',
    searchable_text text,
    occurred_at timestamptz not null,
    recorded_at timestamptz not null default now(),
    unique (source_host, source_user, source_exchange_id)
);
create index activity_exchanges_time_idx on activity_exchanges (occurred_at desc);
create index activity_exchanges_session_idx on activity_exchanges (session_id, occurred_at asc);
create index activity_exchanges_provider_idx on activity_exchanges (provider_id, account_id, occurred_at desc);

-- Upstream attempts per exchange (retries, failovers, account moves).
create table activity_attempts (
    id uuid primary key,
    exchange_id uuid not null references activity_exchanges(id) on delete cascade,
    source_attempt_id text not null,
    attempt_index int not null,
    provider_id text not null,
    account_id text,
    moved_from_account_id text,
    requested_model text not null,
    effective_model text not null,
    effort text,
    status_code int,
    outcome text not null,
    error_message text,
    latency_ms bigint,
    ttft_ms bigint,
    input_tokens bigint not null default 0,
    output_tokens bigint not null default 0,
    cache_read_tokens bigint not null default 0,
    cache_write_tokens bigint not null default 0,
    thinking_tokens bigint not null default 0,
    fast_mode boolean not null default false,
    api_equivalent_cost_usd double precision,
    capture_state text not null default 'none',
    started_at timestamptz not null,
    finished_at timestamptz,
    unique (exchange_id, source_attempt_id)
);
create index activity_attempts_exchange_idx on activity_attempts (exchange_id, attempt_index);

-- Compact durable identity ledger surviving per-request detail expiry so late
-- local imports do not double-count historical totals.
create table activity_identities (
    source_host text not null,
    source_user text not null,
    source_exchange_id text not null,
    canonical_exchange_id uuid not null,
    hour_bucket timestamptz not null,
    contribution jsonb not null,
    recorded_at timestamptz not null default now(),
    primary key (source_host, source_user, source_exchange_id)
);

-- Explicit parent/subagent/fork/compaction session links.
create table activity_session_edges (
    child_session_id text not null,
    parent_session_id text not null,
    edge_kind text not null default 'subagent',
    first_seen_at timestamptz not null default now(),
    primary key (child_session_id, parent_session_id, edge_kind)
);

-- Durable hourly aggregates retained even after per-request details expire.
create table activity_totals (
    hour_bucket timestamptz not null,
    source text not null,
    provider_id text not null,
    account_id text not null default '',
    client text not null default '',
    effective_model text not null,
    effort text not null default '',
    requests bigint not null default 0,
    failures bigint not null default 0,
    input_tokens bigint not null default 0,
    output_tokens bigint not null default 0,
    cache_read_tokens bigint not null default 0,
    cache_write_tokens bigint not null default 0,
    thinking_tokens bigint not null default 0,
    api_equivalent_cost_usd double precision not null default 0,
    unpriced_requests bigint not null default 0,
    primary key (hour_bucket, source, provider_id, account_id, client, effective_model, effort)
);
create index activity_totals_bucket_idx on activity_totals (hour_bucket desc);

-- Separate opt-in captured bodies table (never returned in ordinary request rows).
create table activity_captures (
    id uuid primary key,
    exchange_id uuid not null references activity_exchanges(id) on delete cascade,
    attempt_id uuid references activity_attempts(id) on delete cascade,
    representation text not null,
    redacted boolean not null default true,
    size_bytes bigint not null default 0,
    payload jsonb not null,
    expires_at timestamptz,
    created_at timestamptz not null default now()
);
create index activity_captures_exchange_idx on activity_captures (exchange_id);

-- Drift canary runs & thinking test runs.
create table model_probe_runs (
    id uuid primary key,
    kind text not null check (kind in ('canary', 'thinking_test')),
    provider_id text not null,
    payload jsonb not null,
    ran_at timestamptz not null default now()
);
create index model_probe_runs_kind_idx on model_probe_runs (kind, ran_at desc);

-- Durable pinned eval cases & replay results.
create table model_eval_cases (
    id uuid primary key,
    name text not null,
    source_exchange_id text,
    provider_id text not null,
    model_id text not null,
    effort text,
    prompt_text text not null,
    expected_outcome text not null default 'ok',
    predicates jsonb not null default '[]',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table model_eval_replays (
    id uuid primary key,
    case_id uuid not null references model_eval_cases(id) on delete cascade,
    model_id text not null,
    effort text,
    account_id text,
    actual_outcome text not null,
    response_preview text not null,
    passed boolean not null,
    predicate_results jsonb not null default '[]',
    latency_ms bigint,
    replayed_at timestamptz not null default now()
);
create index model_eval_replays_case_idx on model_eval_replays (case_id, replayed_at desc);

-- Seed default provider settings & capture policy rows in `settings`.
insert into settings (key, value) values
    ('provider_panel_settings', '{"refresh_interval_secs":30,"mask_emails":true,"forecast_horizon_hours":12,"drain_lookback_hours":6,"account_sort":"soonest_reset","notify_limit_hit":true,"notify_limit_reset":true,"notify_account_switch":true,"notify_proxy_crash":true,"tray_slots":[]}'),
    ('provider_capture_policy', '{"capture_bodies_enabled":false,"detail_retention_days":35,"body_retention_days":7,"body_budget_mib":100,"health":{"stored_bodies":0,"stored_bytes":0,"gaps":0,"dropped":0,"rejected":0,"over_budget_evictions":0}}'),
    ('provider_canary_schedule', '{"enabled":false,"interval_hours":12,"targets":[]}'),
    ('provider_clickhouse_status', '{"config":{"enabled":false,"export_content":false,"export_raw_wire":false,"read_through_enabled":false},"connected":false,"queued_outbox_rows":0}')
on conflict (key) do nothing;
