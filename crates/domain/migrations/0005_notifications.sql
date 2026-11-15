-- Notifications: one row per condition, keyed by the producer's dedupe key.
-- `seq` is the long-poll cursor and is bumped on every change; the state is
-- three nullable timestamps (`read_at` = acknowledged, `resolved_at` = the
-- producer saw the condition clear, `snoozed_until` = hidden until then).
-- Producers are level-triggered: each pass upserts the current set per source
-- and resolves what disappeared, so an unchanged condition touches nothing.
create sequence notifications_seq;

create table notifications (
    id uuid primary key,
    company_id text not null,
    -- Bumped on every change; the long-poll cursor.
    seq bigint not null default nextval('notifications_seq'),
    source text not null,          -- today | fleet | hub | summary | agent
    dedupe_key text not null check (dedupe_key ~ '^[a-z0-9][a-z0-9:._-]{0,127}$'),
    severity text not null check (severity in ('info', 'warning', 'critical')),
    title text not null,
    body text not null default '',
    subject_type text,             -- same vocabulary as TodayItem: content, relationship, renewal, ...
    subject_id text,
    url text,
    read_at timestamptz,           -- operator acknowledged
    resolved_at timestamptz,       -- producer saw the condition clear
    snoozed_until timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (company_id, dedupe_key)
);
create index notifications_seq_idx on notifications (company_id, seq);
