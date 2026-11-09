-- infernix-hub initial schema. PostgreSQL owns hub metadata; media lives in
-- the asset store; foreign systems are referenced by id only.

create table principals (
    id uuid primary key,
    name text not null,
    role text not null check (role in ('operator', 'agent', 'procedure', 'ingest')),
    token_sha256 text not null unique,
    created_at timestamptz not null default now()
);

create table settings (
    key text primary key,
    value jsonb not null,
    updated_at timestamptz not null default now()
);
insert into settings (key, value) values ('production_pause', '{"paused": false}');

create table audit_log (
    id bigserial primary key,
    at timestamptz not null default now(),
    actor text not null,
    action text not null,
    subject_type text not null,
    subject_id text not null,
    correlation_id text,
    detail jsonb not null default '{}'
);
create index audit_log_subject on audit_log (subject_type, subject_id, id desc);

-- Inbound events are persisted here before the sender is acknowledged.
create table inbox (
    id uuid primary key,
    source text not null,
    idempotency_key text not null,
    kind text not null,
    payload jsonb not null,
    received_at timestamptz not null default now(),
    deliveries int not null default 1,
    unique (source, idempotency_key)
);

-- Outbound commands, written in the same transaction as the state change.
create table outbox (
    id uuid primary key,
    topic text not null,
    idempotency_key text not null unique,
    payload jsonb not null,
    state text not null default 'pending' check (state in ('pending', 'delivered', 'dead')),
    attempts int not null default 0,
    next_attempt_at timestamptz not null default now(),
    last_error text,
    correlation_id text,
    created_at timestamptz not null default now(),
    delivered_at timestamptz
);
create index outbox_due on outbox (next_attempt_at) where state = 'pending';

create table personas (
    id uuid primary key,
    company_id text not null,
    name text not null,
    created_at timestamptz not null default now()
);

-- Canon is append-only. Chat/news/agents can only create `proposed` rows;
-- only an operator decision makes a revision `accepted`.
create table canon_revisions (
    id uuid primary key,
    persona_id uuid not null references personas (id),
    revision int not null,
    base_revision int,
    body jsonb not null,
    state text not null check (state in ('proposed', 'accepted', 'rejected', 'superseded')),
    source text not null check (source in ('operator', 'chat', 'news', 'agent')),
    rationale text,
    proposed_by text not null,
    decided_by text,
    decided_at timestamptz,
    created_at timestamptz not null default now(),
    unique (persona_id, revision)
);
create unique index canon_one_accepted on canon_revisions (persona_id) where state = 'accepted';

create table news_items (
    id uuid primary key,
    canonical_url text not null unique,
    title text not null,
    summary text not null default '',
    revision int not null default 1,
    first_seen_at timestamptz not null default now(),
    search tsvector generated always as (
        setweight(to_tsvector('simple', coalesce(title, '')), 'A') ||
        setweight(to_tsvector('simple', coalesce(summary, '')), 'B')
    ) stored
);
create index news_search on news_items using gin (search);

create table news_revisions (
    id uuid primary key,
    news_item_id uuid not null references news_items (id),
    revision int not null,
    content_sha256 text not null,
    payload jsonb not null,
    received_at timestamptz not null default now(),
    unique (news_item_id, revision),
    unique (news_item_id, content_sha256)
);

-- One row per distinct (source, external id) that reported this item.
create table news_sightings (
    news_item_id uuid not null references news_items (id),
    source text not null,
    external_id text not null,
    deliveries int not null default 1,
    seen_at timestamptz not null default now(),
    primary key (source, external_id)
);

create table content_items (
    id uuid primary key,
    persona_id uuid not null references personas (id),
    news_item_id uuid references news_items (id),
    title text not null,
    brief jsonb not null,
    state text not null,
    paperclip_issue_id text,
    paperclip_issue_url text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    search tsvector generated always as (
        setweight(to_tsvector('simple', coalesce(title, '')), 'A') ||
        setweight(to_tsvector('simple', coalesce(brief::text, '')), 'C')
    ) stored
);
create index content_search on content_items using gin (search);

create table engine_instances (
    id text primary key,
    kind text not null check (kind in ('comfyui')),
    base_url text not null,
    capabilities jsonb not null default '{}',
    updated_at timestamptz not null default now()
);

create table workflows (
    id uuid primary key,
    name text not null,
    version int not null,
    engine_instance_id text not null references engine_instances (id),
    api_graph jsonb not null,
    graph_sha256 text not null,
    bindings jsonb not null,
    windmill_path text not null,
    unique (name, version)
);

create table generation_requests (
    id uuid primary key,
    content_item_id uuid not null references content_items (id),
    workflow_id uuid not null references workflows (id),
    request_version int not null,
    inputs jsonb not null,
    variants int not null check (variants between 1 and 8),
    -- Hub-chosen ComfyUI prompt ids, known before submission.
    prompt_ids jsonb not null,
    state text not null,
    created_by text not null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table execution_attempts (
    id uuid primary key,
    generation_request_id uuid not null references generation_requests (id),
    parent_id uuid references execution_attempts (id),
    kind text not null check (kind in ('windmill_root', 'windmill_child', 'comfy_prompt')),
    external_id text not null,
    engine_instance_id text,
    state text not null,
    progress jsonb not null default '{}',
    raw jsonb not null default '{}',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (kind, external_id)
);

create table assets (
    id uuid primary key,
    content_item_id uuid not null references content_items (id),
    generation_request_id uuid references generation_requests (id),
    sha256 text not null,
    storage_key text not null,
    mime text not null,
    bytes bigint not null,
    state text not null check (state in ('importing', 'ready', 'failed')),
    selected boolean not null default false,
    alt_text text not null default '',
    -- Provenance copied at import so engine history retention cannot erase it.
    provenance jsonb not null,
    -- Natural key of the engine output, so a re-import is idempotent.
    source_key text not null unique,
    created_at timestamptz not null default now()
);

create table accounts (
    id text primary key,
    company_id text not null,
    platform text not null,
    handle text not null,
    transport text not null,
    persona_id uuid references personas (id),
    capabilities jsonb not null
);

create table release_revisions (
    id uuid primary key,
    content_item_id uuid not null references content_items (id),
    revision int not null,
    actions jsonb not null,
    payload_hash text not null,
    state text not null check (state in ('draft', 'in_review', 'approved', 'rejected', 'superseded')),
    paperclip_approval_id text unique,
    approved_by text,
    approved_at timestamptz,
    created_by text not null,
    created_at timestamptz not null default now(),
    unique (content_item_id, revision)
);

-- One stable intent per (release revision, destination account).
create table publication_intents (
    id uuid primary key,
    release_revision_id uuid not null references release_revisions (id),
    account_id text not null references accounts (id),
    schedule_revision int not null default 1,
    due_at timestamptz not null,
    state text not null,
    provider_post_id text,
    provider_url text,
    claimed_by text,
    claimed_at timestamptz,
    updated_at timestamptz not null default now(),
    unique (release_revision_id, account_id)
);

create table delivery_attempts (
    id uuid primary key,
    intent_id uuid not null references publication_intents (id),
    attempt int not null,
    idempotency_key text not null unique,
    trigger_id text not null,
    state text not null,
    detail jsonb not null default '{}',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (intent_id, attempt)
);

create table relationships (
    id uuid primary key,
    account_id text not null references accounts (id),
    external_thread_id text not null,
    counterpart text not null,
    owner text not null check (owner in ('agent', 'human')),
    owner_user text,
    owner_epoch int not null default 1,
    follow_up_at timestamptz,
    last_inbound_at timestamptz,
    content_item_id uuid references content_items (id),
    permalink text,
    unique (account_id, external_thread_id)
);

create table interactions (
    id uuid primary key,
    relationship_id uuid not null references relationships (id),
    direction text not null check (direction in ('inbound', 'outbound')),
    external_id text not null,
    author text not null,
    body text not null,
    occurred_at timestamptz not null,
    raw jsonb not null default '{}',
    unique (relationship_id, direction, external_id)
);

create table reply_drafts (
    id uuid primary key,
    relationship_id uuid not null references relationships (id),
    in_reply_to uuid references interactions (id),
    body text not null,
    author_kind text not null check (author_kind in ('agent', 'human')),
    author text not null,
    owner_epoch int not null,
    state text not null,
    reviewed_by text,
    provider_reply_id text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);

create table chat_messages (
    id uuid primary key,
    subject_type text not null check (subject_type in ('content', 'persona')),
    subject_id uuid not null,
    role text not null check (role in ('operator', 'assistant', 'system')),
    author text not null,
    body text not null,
    proposal_id uuid references canon_revisions (id),
    created_at timestamptz not null default now()
);
create index chat_subject on chat_messages (subject_type, subject_id, created_at);
