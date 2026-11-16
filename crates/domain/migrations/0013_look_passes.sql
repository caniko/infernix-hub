alter table generation_requests alter column content_item_id drop not null;
alter table generation_requests add column persona_id uuid references personas (id);
alter table generation_requests add column source_asset_id uuid references assets (id);
-- The generation whose id seeded this one, so reused seeds stay the same along a chain.
alter table generation_requests add column seed_origin uuid references generation_requests (id);
alter table generation_requests add constraint generation_requests_one_owner
    check (num_nonnulls(content_item_id, persona_id) = 1);
alter table assets alter column content_item_id drop not null;
alter table assets add column persona_id uuid references personas (id);
alter table assets add constraint assets_one_owner
    check (num_nonnulls(content_item_id, persona_id) = 1);
create index generation_requests_persona on generation_requests (persona_id) where persona_id is not null;
create index assets_persona on assets (persona_id) where persona_id is not null;
