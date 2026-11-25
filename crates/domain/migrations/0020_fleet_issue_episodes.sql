create table fleet_issue_episodes (
    id uuid primary key,
    company_id text not null,
    dedupe_key text not null,
    condition_key text not null,
    notification_id uuid not null references notifications(id) on delete cascade,
    state text not null default 'open' check (state in ('open', 'resolved')),
    paperclip_issue_id text,
    paperclip_issue_url text,
    last_synced_body_hash text not null,
    last_synced_body text not null default '',
    last_synced_severity text not null default 'warning' check (last_synced_severity in ('info', 'warning', 'critical')),
    opened_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    resolved_at timestamptz,
    check ((state = 'open' and resolved_at is null) or (state = 'resolved' and resolved_at is not null))
);

create unique index fleet_issue_episodes_open_idx on fleet_issue_episodes (company_id, condition_key) where state = 'open';
create index fleet_issue_episodes_notification_idx on fleet_issue_episodes (notification_id);
