-- When a row entered its current state. `updated_at` moves on every progress
-- poll, so it cannot time a stall; this moves only when `state` changes, for
-- every writer, because a trigger sets it.
alter table generation_requests add column state_since timestamptz not null default now();
alter table execution_attempts add column state_since timestamptz not null default now();
-- Rows that exist before this migration have no recorded transition time.
-- `updated_at` is refreshed by every poll and would hide a row that is already
-- stalled, so backfill from `created_at`: the earliest the row could have
-- entered its state. This errs toward raising a stall, never toward hiding one.
update generation_requests set state_since = created_at;
update execution_attempts set state_since = created_at;

create function stamp_state_since() returns trigger language plpgsql as $$
begin
    if new.state is distinct from old.state then
        new.state_since := now();
    end if;
    return new;
end
$$;

create trigger generation_requests_state_since before update on generation_requests
    for each row execute function stamp_state_since();
create trigger execution_attempts_state_since before update on execution_attempts
    for each row execute function stamp_state_since();
