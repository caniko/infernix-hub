create table influencers (
    id uuid primary key,
    company_id text not null,
    name text not null check (length(trim(name)) > 0),
    kind text not null check (kind in ('solo', 'group')),
    premise text not null default '',
    dynamics text not null default '',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (company_id, name)
);
create table influencer_members (
    influencer_id uuid not null references influencers (id),
    persona_id uuid not null references personas (id),
    role text not null default '',
    added_at timestamptz not null default now(),
    primary key (influencer_id, persona_id)
);
create index influencer_members_persona on influencer_members (persona_id);
alter table accounts add column influencer_id uuid references influencers (id);
