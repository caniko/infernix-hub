-- A second human user: `member` tokens read and dismiss only their own
-- notifications. Operator rows stay ownerless, so every existing row and every
-- query that does not name an owner keeps its meaning.
alter table principals drop constraint if exists principals_role_check;
alter table principals add constraint principals_role_check
    check (role in ('operator', 'agent', 'procedure', 'ingest', 'member'));

-- null = the operator's rows (as before); a member's rows carry its name and
-- are stored under a `member:<name>:` key prefix, so the existing unique key
-- still separates the two.
alter table notifications add column owner text;
create index notifications_owner_seq_idx on notifications (company_id, owner, seq);
