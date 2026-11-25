alter table projects add constraint projects_company_id_id_key unique (company_id, id);
alter table personas add constraint personas_company_id_id_key unique (company_id, id);

create table intel_sources (
    id text primary key check (id ~ '^[a-z0-9][a-z0-9._-]{0,63}$'),
    company_id text not null,
    policy_version int not null default 1,
    last_ingest_at timestamptz,
    last_event_at timestamptz,
    watermark jsonb not null default '{}',
    last_status text check (last_status in ('ok', 'partial', 'failed')),
    last_error_code text,
    last_error_detail text,
    updated_at timestamptz not null default now(),
    unique (company_id, id)
);

create table intel_deliveries (
    company_id text not null,
    source_id text not null,
    idempotency_key text not null,
    request_sha256 text not null,
    receipt_json jsonb not null,
    created_at timestamptz not null default now(),
    primary key (company_id, source_id, idempotency_key),
    foreign key (company_id, source_id) references intel_sources (company_id, id)
);

create table intel_signals (
    id uuid primary key,
    company_id text not null,
    source_id text not null,
    external_id text not null,
    owner_namespace text not null default '',
    owner text,
    kind text not null check (kind in ('change','metric','mention','announcement','release','funding','partnership','risk','trend','ops')),
    sticky_min_audience text not null check (sticky_min_audience in ('operator_only','company_internal','persona_eligible','public_cleared')),
    cleared_audience text check (cleared_audience in ('operator_only','company_internal','persona_eligible','public_cleared')),
    cleared_base_audience text check (cleared_base_audience in ('operator_only','company_internal','persona_eligible','public_cleared')),
    cleared_source_default_audience text check (cleared_source_default_audience in ('operator_only','company_internal','persona_eligible','public_cleared')),
    cleared_revision int,
    cleared_policy_version int,
    cleared_sha256 text,
    severity text not null check (severity in ('info','warning','critical')),
    dismissed_severity text check (dismissed_severity in ('info','warning','critical')),
    title text not null check (length(title) between 1 and 200),
    summary text not null default '' check (length(summary) <= 4000),
    facts jsonb not null default '{}',
    subjects jsonb not null default '[]',
    evidence jsonb not null check (jsonb_array_length(evidence) > 0),
    confidence real not null check (confidence between 0 and 1),
    provenance jsonb not null,
    occurred_at timestamptz not null,
    expires_at timestamptz,
    revision int not null default 1,
    projection_sha256 text not null,
    state text not null default 'new' check (state in ('new','acknowledged','promoted','dismissed')),
    snoozed_until timestamptz,
    operator_note text,
    persona_id uuid,
    project_id uuid,
    first_seen_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    search tsvector generated always as (setweight(to_tsvector('simple', title), 'A') || setweight(to_tsvector('simple', summary), 'B')) stored,
    unique (company_id, source_id, owner_namespace, external_id),
    unique (company_id, id),
    foreign key (company_id, source_id) references intel_sources (company_id, id),
    foreign key (company_id, persona_id) references personas (company_id, id),
    foreign key (company_id, project_id) references projects (company_id, id),
    check (owner is null or (sticky_min_audience = 'operator_only' and cleared_audience is null)),
    check ((owner is null) = (owner_namespace = '')),
    check (cleared_audience is null or (cleared_base_audience is not null and cleared_revision is not null
        and cleared_source_default_audience is not null and cleared_policy_version is not null and cleared_sha256 is not null))
);

create index intel_signals_audience on intel_signals (company_id, sticky_min_audience, occurred_at desc);
create index intel_signals_open on intel_signals (company_id, source_id, severity) where state = 'new';
create index intel_signals_search on intel_signals using gin (search);
create index intel_signals_subjects on intel_signals using gin (subjects jsonb_path_ops);
create index intel_ingest_budget on audit_log (subject_id, at) where subject_type = 'intel_source' and action = 'intel.ingest';

create table intel_signal_revisions (
    signal_id uuid not null references intel_signals (id),
    revision int not null,
    projection_sha256 text not null,
    payload jsonb not null,
    received_at timestamptz not null default now(),
    primary key (signal_id, revision)
);

create table intel_signal_links (
    signal_id uuid not null references intel_signals (id),
    target_type text not null check (target_type in ('news','opportunity','relationship','content','project','persona','mail_message','signal')),
    target_id text not null,
    target_owner text,
    relation text not null check (relation in ('about','derived_from','promoted_to','supersedes')),
    logical_promotion_key text,
    created_by text not null,
    created_at timestamptz not null default now(),
    primary key (signal_id, target_type, target_id, relation),
    unique nulls not distinct (signal_id, logical_promotion_key)
);
