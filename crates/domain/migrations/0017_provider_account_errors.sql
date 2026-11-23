-- Last structured account failure, separate from quota cooldown.
alter table model_accounts
    add column if not exists error_kind text,
    add column if not exists error_source text,
    add column if not exists error_http_status integer,
    add column if not exists error_code text,
    add column if not exists error_message text,
    add column if not exists error_action_hint text,
    add column if not exists error_action_url text,
    add column if not exists error_at timestamptz,
    add column if not exists error_consecutive_count integer not null default 0,
    add column if not exists error_backoff_until timestamptz,
    add column if not exists observation_batch_at timestamptz,
    add column if not exists publisher_host text,
    add column if not exists publisher_user text,
    add column if not exists source_publisher text;

create table model_provider_publishers (
    provider_id text not null references model_provider_instances(id) on delete cascade,
    source_host text not null,
    source_user text not null,
    source_publisher text not null,
    last_observed_at timestamptz not null,
    primary key (provider_id, source_host, source_user, source_publisher)
);
