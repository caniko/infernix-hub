-- Guide pages a renewal opens in the browser. Part of the seed's facts, like
-- the procedure text, so a re-seed refreshes them.
alter table renewals add column urls text[] not null default '{}';
