create table image_checks (
    asset_id uuid not null references assets (id),
    check_key text not null check (check_key in
        ('adult_or_non_human', 'not_sexualised', 'not_real_person', 'stylized', 'three_d_ready')),
    verdict text not null check (verdict in ('pass', 'fail')),
    checker text not null,            -- 'operator:<name>' now; a model id after slice 8
    detail text not null default '',
    checked_at timestamptz not null default now(),
    primary key (asset_id, check_key, checker)
);
create table persona_references (
    id uuid primary key,
    persona_id uuid not null references personas (id),
    asset_id uuid not null references assets (id),
    pinned_by text not null,
    pinned_at timestamptz not null default now()
);
create index persona_references_current on persona_references (persona_id, pinned_at desc);
