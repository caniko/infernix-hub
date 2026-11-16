create table projects (
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
create table project_members (
    project_id uuid not null references projects (id),
    persona_id uuid not null references personas (id),
    role text not null default '',
    added_at timestamptz not null default now(),
    primary key (project_id, persona_id)
);
create index project_members_persona on project_members (persona_id);
alter table accounts add column project_id uuid references projects (id);
