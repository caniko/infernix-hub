-- Renewals: what the operator has to re-authorize before a channel stops
-- working. `key` is the seed's stable identity; `due_at` null means unknown
-- (renewing starts the clock), and a re-seed never moves either timestamp.
create table renewals (
    id uuid primary key,
    company_id text not null,
    -- A slug, so it is one URL path segment (`/renewals/{key}/renewed`).
    key text not null check (key ~ '^[a-z0-9][a-z0-9._-]*$'),
    account_id text references accounts (id),
    title text not null,
    procedure text not null default '',
    cadence_days int check (cadence_days > 0),
    warn_days int not null default 14 check (warn_days >= 0),
    due_at timestamptz,
    renewed_at timestamptz,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (company_id, key)
);
