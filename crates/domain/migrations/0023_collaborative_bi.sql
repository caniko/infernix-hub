alter table mail_accounts
    add column visibility text not null default 'private'
    check (visibility in ('private', 'shared'));

-- Account lookups already use owner/id; index only the small shared subset.
create index idx_mail_accounts_shared on mail_accounts (id) where visibility = 'shared';

alter table intel_signals add column clearance_generation bigint not null default 0;
update intel_signals set clearance_generation = 1 where cleared_audience is not null and clearance_generation = 0;

-- Existing feed delivery keys remain replayable and cannot collide with promotions.
alter table intel_deliveries
    add column operation text not null default 'ingest' check (operation in ('ingest', 'promotion')),
    drop constraint intel_deliveries_pkey,
    add primary key (company_id, source_id, operation, idempotency_key);
