create table soul_memories (
    id uuid primary key,
    subjects uuid[] not null default '{}',
    tier text not null check (tier in ('canon', 'aired', 'sandbox')),
    relationship_id uuid references relationships (id) on delete set null,
    title text not null default '',
    body text not null,
    keywords text[] not null default '{}',
    salience real not null default 0.5 check (salience >= 0.0 and salience <= 1.0),
    embedding real[],
    occurred_at timestamptz not null default now(),
    created_at timestamptz not null default now(),
    search_tsv tsvector generated always as (to_tsvector('english', coalesce(title, '') || ' ' || body)) stored
);
create index soul_memories_subjects on soul_memories using gin (subjects);
create index soul_memories_keywords on soul_memories using gin (keywords);
create index soul_memories_search on soul_memories using gin (search_tsv);
create index soul_memories_tier_occurred on soul_memories (tier, occurred_at desc);
create index soul_memories_relationship on soul_memories (relationship_id) where relationship_id is not null;

create table soul_voice_exemplars (
    id uuid primary key,
    persona_id uuid not null references personas (id) on delete cascade,
    mode text not null check (mode in ('spoken', 'written')),
    draft text,
    rewrite text not null,
    embedding real[],
    approved_by text not null,
    created_at timestamptz not null default now()
);
create index soul_voice_exemplars_persona_mode_created on soul_voice_exemplars (persona_id, mode, created_at desc);

create table soul_persona_state (
    id uuid primary key,
    persona_id uuid not null references personas (id) on delete cascade,
    state_day date not null,
    mood text not null default '',
    obsessions jsonb not null default '[]'::jsonb,
    ruptures jsonb not null default '[]'::jsonb,
    extra jsonb not null default '{}'::jsonb,
    updated_at timestamptz not null default now(),
    unique (persona_id, state_day)
);
