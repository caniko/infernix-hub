alter table personas add column profile jsonb not null default '{}';
alter table canon_revisions add column review_requested_at timestamptz;
alter table canon_revisions add column review_issue_id text;
alter table canon_revisions add column review_issue_url text;
create table canon_comments (
    id uuid primary key,
    revision_id uuid not null references canon_revisions (id),
    author text not null,
    author_role text not null check (author_role in ('agent', 'operator')),
    body text not null check (length(trim(body)) between 1 and 16384),
    created_at timestamptz not null default now()
);
create index canon_comments_revision on canon_comments (revision_id, created_at);
