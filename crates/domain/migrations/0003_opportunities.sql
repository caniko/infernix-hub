-- Opportunities: one funding call, fellowship, prize, venue or programme per
-- row. A scout files them through ingest; only the operator decides.
--
-- `state` is the operator's trail and ingest never writes it. `expired` is
-- derived on read (a passed deadline while the call is still open), so no
-- timer has to rewrite rows.

create table opportunities (
    id uuid primary key,
    company_id text not null,
    persona_id uuid references personas (id),
    source text not null,
    external_id text not null,
    kind text not null check (kind in ('grant', 'fellowship', 'prize', 'venue', 'program', 'other')),
    title text not null,
    funder text not null,
    url text not null,
    deadline timestamptz,
    amount text,
    eligibility text not null default '',
    fit text not null default '',
    evidence jsonb not null check (jsonb_array_length(evidence) > 0),
    state text not null default 'new'
        check (state in ('new', 'shortlisted', 'drafting', 'submitted', 'awarded', 'rejected', 'declined')),
    note text,
    paperclip_issue_id text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    unique (company_id, source, external_id)
);

-- The deadline queues (`list --due-within`, `/v1/today`).
create index opportunities_due on opportunities (deadline) where state in ('new', 'shortlisted', 'drafting');
