-- Expiry kinds and provider observations (D14-D17). `expiry_kind` is the
-- seed's fact: a cadence clock, an explicit never, or a provider-reported
-- expiry. The observation columns belong to the collector's reports; a re-seed
-- never touches them, and nothing here can move `due_at` or `renewed_at`.
alter table renewals
  add column expiry_kind text not null default 'cadence' check (expiry_kind in ('cadence', 'never', 'provider')),
  add column observed_at timestamptz,               -- last attempt the collector reported
  add column observation text check (observation in ('date', 'never', 'unverifiable', 'inactive', 'not_yet_valid')),
  add column observed_expires_at timestamptz,       -- set only with observation = 'date'
  add column last_good_at timestamptz,              -- last 'date' or 'never' observation
  add column last_good_expires_at timestamptz,      -- its expiry (null for never)
  add column last_good_never boolean not null default false;
