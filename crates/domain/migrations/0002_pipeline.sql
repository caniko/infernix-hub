-- Pipeline generation: an additive second backend next to ComfyUI.
--
-- A workflow is either a ComfyUI workflow (engine + api_graph) or a pipeline
-- workflow (pipeline_id, no graph). For a pipeline workflow the engine is
-- optional and names the admission target; a null target admits immediately.

alter table workflows add column pipeline_id text;
alter table workflows alter column engine_instance_id drop not null;
alter table workflows alter column api_graph drop not null;
alter table workflows
    add constraint workflows_pipeline_shape check ((pipeline_id is null) = (api_graph is not null));
alter table workflows
    add constraint workflows_engine_or_pipeline check (pipeline_id is not null or engine_instance_id is not null);

-- 0001 declared the kind check inline, so PostgreSQL named it
-- execution_attempts_kind_check.
alter table execution_attempts drop constraint execution_attempts_kind_check;
alter table execution_attempts
    add constraint execution_attempts_kind_check
    check (kind in ('windmill_root', 'windmill_child', 'comfy_prompt', 'pipeline_run'));

-- A row is created by whichever arrives first: the upload (with asset_id) or
-- the terminal manifest (without). Completion requires every manifest row to
-- have an asset_id whose stored hash matches.
create table generation_artifacts (
    generation_request_id uuid not null references generation_requests (id),
    source text not null,
    sha256 text not null,
    mime text not null,
    size bigint not null,
    asset_id uuid references assets (id),
    created_at timestamptz not null default now(),
    primary key (generation_request_id, source)
);
