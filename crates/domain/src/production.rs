//! Production: registered ComfyUI workflows and pipelines, versioned
//! generation requests, external receipts, reconciliation and durable asset
//! import.

use contracts::{
    ArtifactRef, Asset, AttemptKind, AttemptState, CancelOut, ContentState, GenerateIn, GenerationRequest, GenerationState,
    JobStatus as WireStatus, ReceiptIn, Role, SelectAssetIn, Workflow,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ports::{
    CancelOutcome, GenerationJob, JobStatus, OutputFile, PipelineRequest, PipelineRun, PipelineStatus, PortError, ProgressEvent,
    PromptOutcome,
};
use crate::{Actor, DResult, DomainError, Hub, OPERATOR_OR_AGENT, audit, conflict, decode, decode_one, hash, invalid, outbox};

/// Version of the `GenerationJob` payload sent to the Windmill procedure.
pub const GENERATION_REQUEST_VERSION: i32 = 1;

/// Version of the `PipelineRequest` payload (spec: request_version = 1).
pub const PIPELINE_REQUEST_VERSION: i32 = 1;

/// Outbox topic that submits one `pipeline_run` attempt. Packs cannot claim
/// it: a pack topic id always contains `<pack id>.` and this one has no dot.
pub const PIPELINE_RUN: &str = "pipeline_run";

/// How long a recorded pipeline manifest may wait for artifact uploads before
/// the generation fails, naming the missing sources.
const ARTIFACT_UPLOAD_DEADLINE_SECS: i64 = 1800;

/// Recorded on an attempt whose submission was claimed (committed before the
/// external call) but whose run handle never arrived. Submitting again could
/// run the work twice, so the operator re-requests after checking the backend.
const SUBMISSION_UNKNOWN: &str = "submission outcome unknown; re-request after checking Windmill";

/// A registry entry (`registry/comfyui/*.json`, `registry/pipeline/*.json`).
/// A ComfyUI workflow has a graph and an engine; a pipeline workflow has a
/// `pipeline_id` and only names an engine as its admission target.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowSpec {
    pub name: String,
    pub version: i32,
    #[serde(default)]
    pub engine_instance_id: Option<String>,
    #[serde(default)]
    pub pipeline_id: Option<String>,
    pub windmill_path: String,
    #[serde(default)]
    pub bindings: Value,
    #[serde(default)]
    pub graph: Option<Value>,
}

/// Outbox payload for one pipeline submission.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PipelineJob {
    /// The `pipeline_run` attempt `generate` recorded for this run.
    pub attempt_id: Uuid,
    pub request: PipelineRequest,
}

/// What the worker does with a claimed pipeline submission row.
#[derive(Debug, PartialEq, Eq)]
pub enum PipelineDelivery {
    Done,
    Retry(String),
    Dead(String),
}

pub async fn register_engine(hub: &Hub, id: &str, base_url: &str, capabilities: Value) -> DResult<()> {
    sqlx::query(
        "insert into engine_instances (id, kind, base_url, capabilities) values ($1, 'comfyui', $2, $3)
         on conflict (id) do update set base_url = excluded.base_url, capabilities = excluded.capabilities, updated_at = now()",
    )
    .bind(id)
    .bind(base_url)
    .bind(capabilities)
    .execute(&hub.db)
    .await?;
    Ok(())
}

/// Register a workflow version. Versions are immutable: re-registering the
/// same name/version with a different graph (or pipeline) is refused.
pub async fn register_workflow(hub: &Hub, spec: &WorkflowSpec) -> DResult<Uuid> {
    let identity = match (&spec.graph, &spec.pipeline_id) {
        (Some(graph), None) => hash::canonical_json(graph),
        (None, Some(pipeline_id)) => hash::canonical_json(&json!({ "pipeline_id": pipeline_id })),
        _ => return Err(invalid("invalid_workflow", "a workflow needs exactly one of graph or pipeline_id")),
    };
    let graph_sha = hash::sha256_hex(identity.as_bytes());
    let existing: Option<(Uuid, String)> = sqlx::query_as("select id, graph_sha256 from workflows where name = $1 and version = $2")
        .bind(&spec.name)
        .bind(spec.version)
        .fetch_optional(&hub.db)
        .await?;
    if let Some((id, sha)) = existing {
        if sha != graph_sha {
            return Err(conflict(
                "workflow_version_immutable",
                format!("{} v{} already registered with a different graph", spec.name, spec.version),
            ));
        }
        return Ok(id);
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "insert into workflows (id, name, version, engine_instance_id, pipeline_id, api_graph, graph_sha256, bindings, windmill_path)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(&spec.name)
    .bind(spec.version)
    .bind(&spec.engine_instance_id)
    .bind(&spec.pipeline_id)
    .bind(&spec.graph)
    .bind(&graph_sha)
    .bind(&spec.bindings)
    .bind(&spec.windmill_path)
    .execute(&hub.db)
    .await?;
    Ok(id)
}

pub async fn workflows(hub: &Hub, actor: &Actor) -> DResult<Vec<Workflow>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(t) from (select id, name, version, engine_instance_id, graph_sha256, bindings, windmill_path
         from workflows order by name, version desc) t",
    )
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

/// Apply named inputs to an API-format graph. `seed` bindings get a distinct
/// value per variant unless the caller pinned one.
pub fn bind_graph(graph: &Value, bindings: &Value, inputs: &Value, variant: i32, seed_base: u64) -> DResult<Value> {
    let mut g = graph.clone();
    let bindings = bindings.as_object().ok_or_else(|| invalid("invalid_workflow", "bindings must be an object"))?;
    if let Some(given) = inputs.as_object() {
        for key in given.keys() {
            if !bindings.contains_key(key) {
                return Err(invalid("unknown_input", format!("workflow has no input named {key}")));
            }
        }
    }
    for (name, b) in bindings {
        let (Some(node), Some(input)) = (b["node"].as_str(), b["input"].as_str()) else {
            return Err(invalid("invalid_workflow", format!("binding {name} needs node and input")));
        };
        let value = match inputs.get(name) {
            Some(v) => v.clone(),
            None if name == "seed" => json!(seed_base.wrapping_add(variant as u64) % (1 << 48)),
            None => match b.get("default") {
                Some(d) => d.clone(),
                None => return Err(invalid("missing_input", format!("input {name} is required"))),
            },
        };
        let slot = g
            .get_mut(node)
            .and_then(|n| n.get_mut("inputs"))
            .and_then(|i| i.as_object_mut())
            .ok_or_else(|| invalid("invalid_workflow", format!("binding {name} targets missing node {node}")))?;
        slot.insert(input.to_string(), value);
    }
    Ok(g)
}

/// Start a generation request. A ComfyUI workflow is dispatched to its Windmill
/// procedure; a pipeline workflow is queued for `hub.pipelines[pipeline_id]`,
/// whose engine (if any) is only the GPU admission target.
pub async fn generate(hub: &Hub, actor: &Actor, content_id: Uuid, input: GenerateIn) -> DResult<GenerationRequest> {
    actor.require(OPERATOR_OR_AGENT)?;
    if !(1..=8).contains(&input.variants) {
        return Err(invalid("invalid_variants", "variants must be 1..=8"));
    }
    type Wf = (Option<Value>, Value, String, Option<String>, Option<String>, Option<String>);
    let wf: Option<Wf> = sqlx::query_as(
        "select w.api_graph, w.bindings, w.windmill_path, w.engine_instance_id, e.base_url, w.pipeline_id
         from workflows w left join engine_instances e on e.id = w.engine_instance_id where w.id = $1",
    )
    .bind(input.workflow_id)
    .fetch_optional(&hub.db)
    .await?;
    let (graph, bindings, windmill_path, engine_id, engine_url, pipeline_id) = wf.ok_or(DomainError::NotFound("workflow"))?;
    let id = Uuid::new_v4();
    let Some(pipeline_id) = pipeline_id else {
        return start_comfy(hub, actor, content_id, input, id, (graph, bindings, windmill_path, engine_id, engine_url)).await;
    };
    // One pipeline run per request: the pipeline expands variants itself from
    // `inputs`.
    if !hub.pipelines.contains_key(&pipeline_id) {
        return Err(invalid("pipeline_unavailable", format!("no pipeline is registered for {pipeline_id}")));
    }
    let attempt_id = Uuid::new_v4();
    let job = PipelineJob { attempt_id, request: pipeline_request(hub, id, pipeline_id.clone(), input.inputs.clone()) };
    let mut tx = hub.db.begin().await?;
    crate::content::get_item(&mut tx, content_id).await?;
    sqlx::query(
        "insert into generation_requests (id, content_item_id, workflow_id, request_version, inputs, variants, prompt_ids, state, created_by)
         values ($1, $2, $3, $4, $5, $6, '[]', 'requested', $7)",
    )
    .bind(id)
    .bind(content_id)
    .bind(input.workflow_id)
    .bind(PIPELINE_REQUEST_VERSION)
    .bind(&input.inputs)
    .bind(input.variants)
    .bind(actor.name())
    .execute(&mut *tx)
    .await?;
    // The attempt exists before submission so a receipt (which arrives under
    // its own external id) and a cancel can both find it.
    sqlx::query(
        "insert into execution_attempts (id, generation_request_id, kind, external_id, engine_instance_id, state, progress)
         values ($1, $2, 'pipeline_run', $3, $4, 'submitted', $5)",
    )
    .bind(attempt_id)
    .bind(id)
    .bind(&job.request.idempotency_key)
    .bind(&engine_id)
    .bind(json!({ "pipeline_id": pipeline_id }))
    .execute(&mut *tx)
    .await?;
    outbox::enqueue(&mut tx, actor, PIPELINE_RUN, &format!("generation:{id}"), serde_json::to_value(&job).unwrap_or_default()).await?;
    crate::content::set_state(&mut tx, content_id, ContentState::Producing).await?;
    audit(
        &mut tx,
        actor,
        "generation.requested",
        "content",
        content_id,
        json!({ "generation_request_id": id, "workflow_id": input.workflow_id, "pipeline_id": pipeline_id, "variants": input.variants }),
    )
    .await?;
    tx.commit().await?;
    get_generation(hub, id).await
}

type ComfySpec = (Option<Value>, Value, String, Option<String>, Option<String>);

/// The submission payload for one pipeline generation. Every field (stable
/// idempotency key included) comes from the generation itself, not from the
/// caller.
fn pipeline_request(hub: &Hub, generation_id: Uuid, pipeline_id: String, inputs: Value) -> PipelineRequest {
    PipelineRequest {
        request_version: PIPELINE_REQUEST_VERSION,
        generation_request_id: generation_id,
        pipeline_id,
        inputs,
        idempotency_key: format!("generation:{generation_id}:pipeline"),
        receipt_url: format!("{}/v1/generations/{generation_id}/receipts", hub.config.public_url),
        artifact_url: format!("{}/v1/generations/{generation_id}/artifacts", hub.config.public_url),
    }
}

async fn start_comfy(
    hub: &Hub,
    actor: &Actor,
    content_id: Uuid,
    input: GenerateIn,
    id: Uuid,
    spec: ComfySpec,
) -> DResult<GenerationRequest> {
    let (graph, bindings, windmill_path, engine_id, engine_url) = spec;
    let (Some(graph), Some(engine_id), Some(engine_url)) = (graph, engine_id, engine_url) else {
        return Err(invalid("workflow_without_engine", "this ComfyUI workflow has no engine registered"));
    };
    let seed_base = u64::from_le_bytes(id.as_bytes()[..8].try_into().unwrap());
    let prompts = (0..input.variants).map(|v| bind_graph(&graph, &bindings, &input.inputs, v, seed_base)).collect::<DResult<Vec<_>>>()?;
    let prompt_ids: Vec<String> = (0..input.variants).map(|_| Uuid::new_v4().to_string()).collect();

    let mut tx = hub.db.begin().await?;
    crate::content::get_item(&mut tx, content_id).await?;
    sqlx::query(
        "insert into generation_requests (id, content_item_id, workflow_id, request_version, inputs, variants, prompt_ids, state, created_by)
         values ($1, $2, $3, $4, $5, $6, $7, 'requested', $8)",
    )
    .bind(id)
    .bind(content_id)
    .bind(input.workflow_id)
    .bind(GENERATION_REQUEST_VERSION)
    .bind(&input.inputs)
    .bind(input.variants)
    .bind(json!(prompt_ids))
    .bind(actor.name())
    .execute(&mut *tx)
    .await?;
    // Known prompt ids exist before submission so reconciliation can find
    // them even if the procedure's receipt is lost.
    for pid in &prompt_ids {
        sqlx::query(
            "insert into execution_attempts (id, generation_request_id, kind, external_id, engine_instance_id, state)
             values ($1, $2, 'comfy_prompt', $3, $4, 'submitted')",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(pid)
        .bind(&engine_id)
        .execute(&mut *tx)
        .await?;
    }
    let job = GenerationJob {
        request_version: GENERATION_REQUEST_VERSION,
        generation_request_id: id.to_string(),
        windmill_path,
        engine_instance_id: engine_id,
        engine_base_url: engine_url,
        prompts,
        prompt_ids,
        comfy_client_id: comfy_client_id(),
        receipt_url: format!("{}/v1/generations/{id}/receipts", hub.config.public_url),
    };
    outbox::enqueue(
        &mut tx,
        actor,
        outbox::WINDMILL_RUN_GENERATION,
        &format!("generation:{id}"),
        serde_json::to_value(&job).unwrap_or_default(),
    )
    .await?;
    crate::content::set_state(&mut tx, content_id, ContentState::Producing).await?;
    audit(
        &mut tx,
        actor,
        "generation.requested",
        "content",
        content_id,
        json!({ "generation_request_id": id, "workflow_id": input.workflow_id, "variants": input.variants }),
    )
    .await?;
    tx.commit().await?;
    get_generation(hub, id).await
}

/// One stable `/ws` client id per hub process family, so progress for every
/// prompt the hub requested reaches its listener.
pub fn comfy_client_id() -> String {
    std::env::var("HUB_COMFY_CLIENT_ID").unwrap_or_else(|_| "fleetix-hub".into())
}

const GEN_SELECT: &str = "select to_jsonb(t) from (
    select g.id, g.content_item_id, g.workflow_id, g.request_version, g.inputs, g.variants, g.state, g.created_at,
      coalesce((select jsonb_agg(to_jsonb(a) order by a.created_at, a.external_id) from (
         select id, parent_id, kind, external_id, engine_instance_id, state, progress, updated_at, created_at
         from execution_attempts where generation_request_id = g.id) a), '[]') as attempts
    from generation_requests g";

pub async fn get_generation(hub: &Hub, id: Uuid) -> DResult<GenerationRequest> {
    let row: Option<Value> = sqlx::query_scalar(&format!("{GEN_SELECT} where g.id = $1) t")).bind(id).fetch_optional(&hub.db).await?;
    decode_one(row.ok_or(DomainError::NotFound("generation request"))?)
}

pub async fn list_for_content(hub: &Hub, content_id: Uuid) -> DResult<Vec<GenerationRequest>> {
    let rows: Vec<Value> = sqlx::query_scalar(&format!("{GEN_SELECT} where g.content_item_id = $1 order by g.created_at desc) t"))
        .bind(content_id)
        .fetch_all(&hub.db)
        .await?;
    decode(rows)
}

/// Receipt posted by the Windmill procedure, a pipeline, or recorded by the
/// worker. A pipeline receipt may carry a terminal `status` and the complete
/// `artifacts` manifest.
pub async fn record_receipt(hub: &Hub, actor: &Actor, generation_id: Uuid, r: ReceiptIn) -> DResult<GenerationRequest> {
    actor.require(&[Role::Procedure])?;
    if r.external_id.trim().is_empty() {
        return Err(invalid("invalid_receipt", "external_id is required"));
    }
    // A succeeded run is only terminal once its complete manifest is recorded.
    // `ReceiptIn::artifacts` defaults to empty, so a receipt that omits the
    // manifest is refused instead of completing an empty generation.
    if r.kind == AttemptKind::PipelineRun && r.status == Some(WireStatus::Succeeded) {
        validate_manifest(&r.artifacts)?;
    }
    let mut tx = hub.db.begin().await?;
    // The request row is locked for the whole receipt: the attempt write and
    // the state derived from it cannot interleave with a cancel.
    let generation_state = lock_generation(&mut tx, generation_id).await?;
    // A pipeline posts one receipt for the run `generate` already recorded;
    // identify it by request, not by the backend's external id.
    let pipeline_attempt: Option<Uuid> = if r.kind == AttemptKind::PipelineRun {
        sqlx::query_scalar(
            "select id from execution_attempts where generation_request_id = $1 and kind = 'pipeline_run' order by created_at limit 1",
        )
        .bind(generation_id)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        None
    };
    let (attempt_id, owner, state): (Uuid, Uuid, String) = match pipeline_attempt {
        Some(id) => {
            sqlx::query_as("select id, generation_request_id, state from execution_attempts where id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?
        }
        None => {
            let parent: Option<Uuid> = match &r.parent_external_id {
                Some(p) => sqlx::query_scalar(
                    "select id from execution_attempts where generation_request_id = $1 and external_id = $2 and kind <> 'comfy_prompt'",
                )
                .bind(generation_id)
                .bind(p)
                .fetch_optional(&mut *tx)
                .await?,
                None => None,
            };
            sqlx::query_as(
                "insert into execution_attempts (id, generation_request_id, parent_id, kind, external_id, engine_instance_id, state, raw)
                 values ($1, $2, $3, $4, $5, $6, 'submitted', $7)
                 on conflict (kind, external_id) do update
                   set parent_id = coalesce(excluded.parent_id, execution_attempts.parent_id),
                       engine_instance_id = coalesce(excluded.engine_instance_id, execution_attempts.engine_instance_id),
                       raw = excluded.raw, updated_at = now()
                 returning id, generation_request_id, state",
            )
            .bind(Uuid::new_v4())
            .bind(generation_id)
            .bind(parent)
            .bind(r.kind.as_str())
            .bind(&r.external_id)
            .bind(&r.engine_instance_id)
            .bind(&r.raw)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    if owner != generation_id {
        return Err(conflict("receipt_owner_mismatch", "external id belongs to a different generation request"));
    }
    if let Some(status) = r.status
        && r.kind == AttemptKind::PipelineRun
        && status != WireStatus::Succeeded
    {
        // `succeeded` is only recorded once its manifest is durable, below.
        let state = match status {
            WireStatus::Queued => AttemptState::Queued,
            WireStatus::Running => AttemptState::Running,
            WireStatus::Succeeded => AttemptState::Succeeded,
            WireStatus::Failed => AttemptState::Failed,
            WireStatus::Cancelled => AttemptState::Cancelled,
        };
        set_attempt_in(&mut *tx, attempt_id, state, json!({ "receipt": r.external_id })).await?;
    }
    sqlx::query(
        "update generation_requests set state = 'running', updated_at = now() where id = $1 and state in ('requested', 'dispatched')",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    if r.kind == AttemptKind::PipelineRun {
        // A pipeline receipt can itself be terminal (failed, cancelled). The
        // attempt write above and the request state derived from it commit
        // together, under the lock taken at the top.
        write_derived_state(&mut tx, generation_id, generation_state).await?;
    }
    audit(&mut tx, actor, "generation.receipt", "generation", generation_id, json!({ "kind": r.kind, "external_id": r.external_id }))
        .await?;
    tx.commit().await?;
    if r.kind == AttemptKind::PipelineRun {
        // The receipt is authoritative for its own request; there is no status
        // read to fence (the attempt is identified by generation). A manifest
        // ends its attempt and the request in one transaction.
        if r.status == Some(WireStatus::Succeeded) {
            record_manifest(hub, generation_id, attempt_id, None, &r.artifacts).await?;
        }
        return get_generation(hub, generation_id).await;
    }
    // The operator cancelled before the procedure submitted: stop the work
    // that just reached the engine instead of letting it run orphaned.
    if r.kind == AttemptKind::ComfyPrompt && state == AttemptState::Cancelled.as_str() {
        let url: Option<String> = sqlx::query_scalar(
            "select e.base_url from generation_requests g join workflows w on w.id = g.workflow_id
             left join engine_instances e on e.id = w.engine_instance_id where g.id = $1",
        )
        .bind(generation_id)
        .fetch_optional(&hub.db)
        .await?;
        if let Some(url) = url
            && let Err(e) = hub.comfy.cancel(&url, &r.external_id).await
        {
            tracing::warn!(prompt_id = %r.external_id, error = %e, "late cancel of cancelled prompt failed");
        }
        return Err(conflict("attempt_cancelled", "this prompt was cancelled; the engine work was asked to stop"));
    }
    get_generation(hub, generation_id).await
}

/// A manifest digest is stored canonical: 64 lowercase hex characters. An
/// uppercase digest names the same bytes, so it is normalized rather than
/// treated as a different artifact.
fn normalize_sha256(sha: &str) -> Option<String> {
    let sha = sha.trim();
    (sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| sha.to_ascii_lowercase())
}

/// Check a terminal manifest and return it with canonical digests. An empty
/// manifest is refused: a pipeline that produced nothing did not succeed, and
/// `ReceiptIn::artifacts` defaults to empty when the field is absent.
fn validate_manifest(artifacts: &[ArtifactRef]) -> DResult<Vec<ArtifactRef>> {
    if artifacts.is_empty() {
        return Err(invalid("manifest_required", "a succeeded pipeline run must carry its complete artifact manifest"));
    }
    let mut sources = std::collections::BTreeSet::new();
    let mut checked = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let source = artifact.source.trim();
        if source.is_empty() {
            return Err(invalid("invalid_manifest", "each manifest artifact needs a source"));
        }
        if !sources.insert(source.to_string()) {
            return Err(invalid("invalid_manifest", format!("{source} appears twice in the manifest")));
        }
        let sha256 = normalize_sha256(&artifact.sha256)
            .ok_or_else(|| invalid("invalid_manifest", format!("{source}: sha256 must be 64 hex characters")))?;
        if artifact.mime.trim().is_empty() {
            return Err(invalid("invalid_manifest", format!("{source}: mime is required")));
        }
        checked.push(ArtifactRef { source: source.to_string(), sha256, mime: artifact.mime.clone(), size: artifact.size });
    }
    Ok(checked)
}

/// Record the complete manifest of a pipeline run. Uploads may already exist
/// (with their assets) and are part of the run: the manifest must name every
/// one of them. A digest that contradicts an upload fails the run. The
/// generation row is locked for the whole write, so an upload cannot slip in
/// between the check and the record. `status_run` fences a manifest derived
/// from a status read to the run that attempt currently follows.
async fn record_manifest(
    hub: &Hub,
    generation_id: Uuid,
    attempt_id: Uuid,
    status_run: Option<&PipelineRun>,
    artifacts: &[ArtifactRef],
) -> DResult<()> {
    let artifacts = validate_manifest(artifacts)?;
    let mut tx = hub.db.begin().await?;
    let current = lock_generation(&mut tx, generation_id).await?;
    let raw: Option<Value> =
        sqlx::query_scalar("select raw from execution_attempts where id = $1 for update").bind(attempt_id).fetch_optional(&mut *tx).await?;
    let raw = raw.ok_or(DomainError::NotFound("pipeline attempt"))?;
    if let Some(run) = status_run
        && raw.get("run").and_then(|r| r.get("external_id")).and_then(Value::as_str) != Some(run.external_id.as_str())
    {
        // A status read for a run this attempt no longer follows is stale.
        tx.commit().await?;
        return Ok(());
    }
    let wanted: std::collections::BTreeSet<&str> = artifacts.iter().map(|a| a.source.as_str()).collect();
    let recorded: Vec<String> =
        sqlx::query_scalar("select source from generation_artifacts where generation_request_id = $1 order by source")
            .bind(generation_id)
            .fetch_all(&mut *tx)
            .await?;
    if raw.get("manifest_at").is_some() {
        // A terminal manifest is recorded once: only an exact replay is a
        // no-op. A subset or superset contradicts the recorded source set.
        let exact = recorded.len() == wanted.len() && recorded.iter().all(|s| wanted.contains(s.as_str()));
        if !exact {
            return Err(conflict("manifest_conflict", "the replayed manifest does not match the recorded source set"));
        }
        for artifact in &artifacts {
            let stored: Option<String> =
                sqlx::query_scalar("select sha256 from generation_artifacts where generation_request_id = $1 and source = $2")
                    .bind(generation_id)
                    .bind(&artifact.source)
                    .fetch_optional(&mut *tx)
                    .await?;
            if stored.as_deref() != Some(artifact.sha256.as_str()) {
                return Err(conflict("manifest_conflict", format!("{} already has a different terminal manifest", artifact.source)));
            }
        }
        complete_pipeline_in(&mut *tx, generation_id).await?;
        write_derived_state(&mut tx, generation_id, current).await?;
        tx.commit().await?;
        return Ok(());
    }
    // A pre-upload the manifest omits contradicts the run's own output.
    let omitted: Vec<&str> = recorded.iter().map(String::as_str).filter(|source| !wanted.contains(source)).collect();
    if !omitted.is_empty() {
        let names = omitted.join(", ");
        set_attempt_in(
            &mut *tx,
            attempt_id,
            AttemptState::Failed,
            json!({ "error": format!("the manifest omits uploaded artifact(s): {names}") }),
        )
        .await?;
        write_derived_state(&mut tx, generation_id, current).await?;
        tx.commit().await?;
        return Err(conflict("manifest_conflict", format!("{names}: uploaded but not part of the manifest")));
    }
    let mut mismatched = None;
    for artifact in &artifacts {
        let stored: Option<(String, Option<Uuid>)> =
            sqlx::query_as("select sha256, asset_id from generation_artifacts where generation_request_id = $1 and source = $2 for update")
                .bind(generation_id)
                .bind(&artifact.source)
                .fetch_optional(&mut *tx)
                .await?;
        match stored {
            // The upload arrived first: its digest is the truth about the bytes.
            Some((sha, Some(_))) => {
                if sha != artifact.sha256 {
                    mismatched = Some(artifact.source.clone());
                    break;
                }
            }
            // The manifest owns the metadata of a row nobody uploaded yet.
            Some(_) => {
                sqlx::query(
                    "update generation_artifacts set sha256 = $3, mime = $4, size = $5
                     where generation_request_id = $1 and source = $2 and asset_id is null",
                )
                .bind(generation_id)
                .bind(&artifact.source)
                .bind(&artifact.sha256)
                .bind(&artifact.mime)
                .bind(artifact.size as i64)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "insert into generation_artifacts (generation_request_id, source, sha256, mime, size)
                     values ($1, $2, $3, $4, $5)",
                )
                .bind(generation_id)
                .bind(&artifact.source)
                .bind(&artifact.sha256)
                .bind(&artifact.mime)
                .bind(artifact.size as i64)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    if let Some(source) = mismatched {
        set_attempt_in(
            &mut *tx,
            attempt_id,
            AttemptState::Failed,
            json!({ "error": "manifest hash differs from the uploaded artifact", "source": source }),
        )
        .await?;
        write_derived_state(&mut tx, generation_id, current).await?;
        tx.commit().await?;
        return Err(conflict("artifact_hash_mismatch", format!("{source}: the manifest digest differs from the uploaded artifact")));
    }
    let missing: Vec<String> =
        sqlx::query_scalar("select source from generation_artifacts where generation_request_id = $1 and asset_id is null order by source")
            .bind(generation_id)
            .fetch_all(&mut *tx)
            .await?;
    // The attempt stays open until every manifest artifact is uploaded: the
    // upload deadline and a cancel must still be able to end it.
    sqlx::query(
        "update execution_attempts
            set raw = raw || jsonb_build_object('manifest_received', $2, 'manifest_at', $3),
                state = case when state in ('submitted', 'queued') then 'running' else state end, updated_at = now()
          where id = $1 and state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown')",
    )
    .bind(attempt_id)
    .bind(artifacts.len() as i64)
    .bind(chrono::Utc::now().timestamp())
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        &Actor::system(),
        "generation.manifest",
        "generation",
        generation_id,
        json!({ "artifacts": artifacts.len(), "missing": missing }),
    )
    .await?;
    complete_pipeline_in(&mut *tx, generation_id).await?;
    write_derived_state(&mut tx, generation_id, current).await?;
    tx.commit().await?;
    Ok(())
}

/// A pipeline attempt succeeds only when its terminal manifest is recorded and
/// every manifest row has a uploaded, matching, durable asset. Anything less
/// leaves it open for the deadline and cancellation paths.
const COMPLETE_PIPELINE: &str = "update execution_attempts a set state = 'succeeded', updated_at = now()
     where a.generation_request_id = $1 and a.kind = 'pipeline_run'
       and a.state in ('submitted', 'queued', 'running')
       and a.raw ? 'manifest_at'
       and not exists (
         select 1 from generation_artifacts x left join assets s on s.id = x.asset_id
          where x.generation_request_id = a.generation_request_id
            and (x.asset_id is null or s.state <> 'ready' or s.sha256 <> x.sha256))";

async fn complete_pipeline_in(tx: &mut sqlx::PgConnection, generation_id: Uuid) -> DResult<u64> {
    Ok(sqlx::query(COMPLETE_PIPELINE).bind(generation_id).execute(tx).await?.rows_affected())
}

/// End an attempt that cannot be followed any further, then re-derive its
/// request so a failed submission is not stranded in `requested`.
async fn fail_pipeline_attempt(hub: &Hub, generation_id: Uuid, attempt_id: Uuid, error: &str) -> DResult<()> {
    set_attempt_and_refresh(hub, generation_id, attempt_id, AttemptState::Failed, json!({ "error": error })).await
}

/// Undo a submission claim whose request provably never left the hub
/// (`PortError::Unreachable`), so the outbox retry may claim it again.
async fn release_submission_claim(hub: &Hub, attempt_id: Uuid) -> DResult<()> {
    sqlx::query(
        "update execution_attempts set raw = raw - 'submit_started_at', updated_at = now()
          where id = $1 and state = 'submitted' and not (raw ? 'run')",
    )
    .bind(attempt_id)
    .execute(&hub.db)
    .await?;
    Ok(())
}

/// True when a terminal manifest has been recorded for this generation, even if
/// not every artifact has been uploaded yet.
const EXISTS_MANIFEST: &str = "select exists(select 1 from execution_attempts
     where generation_request_id = $1 and kind = 'pipeline_run' and raw ? 'manifest_at')";

/// `POST /v1/generations/{id}/artifacts`: store one uploaded output and link it
/// to its manifest row, whichever of the two arrives first.
pub async fn upload_artifact(
    hub: &Hub,
    actor: &Actor,
    generation_id: Uuid,
    source: &str,
    mime: &str,
    sha256: &str,
    bytes: &[u8],
) -> DResult<GenerationRequest> {
    actor.require(&[Role::Procedure])?;
    let source = source.trim();
    if source.is_empty() {
        return Err(invalid("invalid_artifact", "x-artifact-source is required"));
    }
    let Some(claimed) = normalize_sha256(sha256) else {
        return Err(invalid("invalid_artifact", "x-sha256 must be 64 hex characters"));
    };
    let actual = hash::sha256_hex(bytes);
    if actual != claimed {
        // The caller's own claim about the bytes is wrong: a bad request, not
        // a state conflict.
        return Err(invalid("artifact_hash_mismatch", format!("uploaded bytes hash to {actual}, not the advertised {sha256}")));
    }
    let mut tx = hub.db.begin().await?;
    let row: Option<(Uuid, Option<String>, String)> = sqlx::query_as(
        "select g.id, w.pipeline_id, g.state from generation_requests g join workflows w on w.id = g.workflow_id
         where g.id = $1 for update of g",
    )
    .bind(generation_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (_, pipeline_id, generation_state) = row.ok_or(DomainError::NotFound("generation request"))?;
    let current = GenerationState::parse(&generation_state).unwrap_or(GenerationState::Requested);
    if pipeline_id.is_none() {
        return Err(invalid("artifact_not_supported", "this generation is not a pipeline run"));
    }
    // Once the terminal manifest is recorded it is the complete source list.
    let manifest: bool = sqlx::query_scalar(EXISTS_MANIFEST).bind(generation_id).fetch_one(&mut *tx).await?;
    let recorded: Option<(String, Option<Uuid>)> =
        sqlx::query_as("select sha256, asset_id from generation_artifacts where generation_request_id = $1 and source = $2")
            .bind(generation_id)
            .bind(source)
            .fetch_optional(&mut *tx)
            .await?;
    if manifest && recorded.is_none() {
        return Err(conflict("artifact_not_in_manifest", format!("{source} is not part of this generation's manifest")));
    }
    if let Some((expected, _)) = &recorded
        && expected != &actual
    {
        return Err(conflict("artifact_hash_mismatch", format!("{source} is recorded with sha256 {expected}, not {actual}")));
    }
    let key = hub.assets.put(&actual, bytes).await.map_err(|e| DomainError::Internal(format!("asset store: {e}")))?;
    let source_key = format!("pipeline:{generation_id}:{source}");
    let provenance = json!({
        "pipeline_id": pipeline_id, "generation_request_id": generation_id, "source": source, "storage_key": key,
    });
    let asset_id: Uuid = sqlx::query_scalar(
        "insert into assets (id, content_item_id, generation_request_id, sha256, storage_key, mime, bytes, state, provenance, source_key)
         select $1, g.content_item_id, g.id, $2, $3, $4, $5, 'ready', $6, $7 from generation_requests g where g.id = $8
         on conflict (source_key) do update
           set sha256 = excluded.sha256, storage_key = excluded.storage_key, mime = excluded.mime, bytes = excluded.bytes, state = 'ready'
         returning id",
    )
    .bind(Uuid::new_v4())
    .bind(&actual)
    .bind(&key)
    .bind(mime)
    .bind(bytes.len() as i64)
    .bind(&provenance)
    .bind(&source_key)
    .bind(generation_id)
    .fetch_one(&mut *tx)
    .await?;
    let linked = sqlx::query(
        "insert into generation_artifacts (generation_request_id, source, sha256, mime, size, asset_id)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (generation_request_id, source) do update
           set asset_id = excluded.asset_id
           where generation_artifacts.sha256 = excluded.sha256",
    )
    .bind(generation_id)
    .bind(source)
    .bind(&actual)
    .bind(mime)
    .bind(bytes.len() as i64)
    .bind(asset_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if linked != 1 {
        // The manifest named different bytes for this source. Never report a
        // link that did not happen.
        return Err(conflict("artifact_hash_mismatch", format!("{source} is recorded with a different sha256")));
    }
    let content_id: Uuid =
        sqlx::query_scalar("select content_item_id from generation_requests where id = $1").bind(generation_id).fetch_one(&mut *tx).await?;
    audit(
        &mut tx,
        actor,
        "generation.artifact",
        "content",
        content_id,
        json!({ "generation_request_id": generation_id, "asset_id": asset_id, "source": source, "sha256": actual }),
    )
    .await?;
    // This upload may be the last one the recorded manifest was waiting for.
    complete_pipeline_in(&mut *tx, generation_id).await?;
    write_derived_state(&mut tx, generation_id, current).await?;
    tx.commit().await?;
    get_generation(hub, generation_id).await
}

/// Record the Windmill root job id once the worker has started the procedure.
pub async fn record_dispatched(hub: &Hub, generation_id: Uuid, windmill_job_id: &str) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    // Generation first, like every other attempt writer, so a concurrent
    // receipt cannot hold the root key while waiting for this row.
    lock_generation(&mut tx, generation_id).await?;
    let root_id: Uuid = sqlx::query_scalar(
        "insert into execution_attempts (id, generation_request_id, kind, external_id, state) values ($1, $2, 'windmill_root', $3, 'submitted')
         on conflict (kind, external_id) do update set external_id = excluded.external_id returning id",
    )
    .bind(Uuid::new_v4())
    .bind(generation_id)
    .bind(windmill_job_id)
    .fetch_one(&mut *tx)
    .await?;
    // A prompt receipt can arrive before its root job is recorded: link the
    // orphaned prompts now instead of losing the attempt tree.
    sqlx::query(
        "update execution_attempts set parent_id = $2, updated_at = now()
          where generation_request_id = $1 and kind = 'comfy_prompt' and parent_id is null",
    )
    .bind(generation_id)
    .bind(root_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("update generation_requests set state = 'dispatched', updated_at = now() where id = $1 and state = 'requested'")
        .bind(generation_id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &Actor::system(), "generation.dispatched", "generation", generation_id, json!({ "windmill_job_id": windmill_job_id }))
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Record the pipeline run handle the backend returned for a submitted attempt.
/// The attempt row already exists, so a receipt or a cancel can race this
/// safely. Lock order is generation first, then attempts (as receipts do).
pub async fn record_pipeline_submitted(hub: &Hub, attempt_id: Uuid, run: &PipelineRun) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "select a.generation_request_id, w.pipeline_id from execution_attempts a
         join generation_requests g on g.id = a.generation_request_id join workflows w on w.id = g.workflow_id
         where a.id = $1",
    )
    .bind(attempt_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (generation_id, pipeline_id) = row.ok_or(DomainError::NotFound("pipeline attempt"))?;
    sqlx::query("select id from generation_requests where id = $1 for update").bind(generation_id).execute(&mut *tx).await?;
    // Re-read under the lock: a cancel may have won while submit was in flight.
    let state: String =
        sqlx::query_scalar("select state from execution_attempts where id = $1").bind(attempt_id).fetch_one(&mut *tx).await?;
    if state == AttemptState::Cancelled.as_str() {
        // Never adopt a run the operator already cancelled: stop it instead.
        sqlx::query(
            "update execution_attempts set raw = raw || jsonb_build_object('cancelled_run', $2::jsonb), updated_at = now()
              where id = $1",
        )
        .bind(attempt_id)
        .bind(serde_json::to_value(run).unwrap_or_default())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        match hub.pipelines.get(&pipeline_id) {
            Some(pipeline) => match pipeline.cancel(run).await {
                Ok(outcome) => tracing::info!(run = %run.external_id, ?outcome, "cancelled pipeline run stopped"),
                Err(e) => tracing::warn!(run = %run.external_id, error = %e, "cancelled pipeline run could not be stopped"),
            },
            None => tracing::warn!(%pipeline_id, run = %run.external_id, "no pipeline registered to stop the cancelled run"),
        }
        return Ok(());
    }
    // A terminal state from a fast receipt is never downgraded by this record.
    sqlx::query(
        "update execution_attempts set raw = raw || jsonb_build_object('run', $2::jsonb),
           state = case when state = 'submitted' then 'queued' else state end, updated_at = now() where id = $1",
    )
    .bind(attempt_id)
    .bind(serde_json::to_value(run).unwrap_or_default())
    .execute(&mut *tx)
    .await?;
    sqlx::query("update generation_requests set state = 'dispatched', updated_at = now() where id = $1 and state = 'requested'")
        .bind(generation_id)
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &Actor::system(), "generation.dispatched", "generation", generation_id, json!({ "pipeline_run": run.external_id }))
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Deliver one queued pipeline submission. Submission is at-most-once per
/// attempt: the claim is committed before the external call, so a crash after
/// the backend accepted the run is redelivered as an unknown outcome rather
/// than as a second run.
pub async fn deliver_pipeline(hub: &Hub, job: &PipelineJob) -> DResult<PipelineDelivery> {
    let Some(pipeline) = hub.pipelines.get(&job.request.pipeline_id) else {
        return Ok(PipelineDelivery::Dead(format!("no pipeline is registered for {}", job.request.pipeline_id)));
    };
    // One decision under the generation row lock, serialized with `cancel`:
    // what this delivery must not repeat, and whether it may submit at all.
    let mut tx = hub.db.begin().await?;
    let row: Option<(Uuid, String, Value, String)> = sqlx::query_as(
        "select g.id, a.state, a.raw, g.state from execution_attempts a join generation_requests g on g.id = a.generation_request_id
         where a.id = $1 for update of g",
    )
    .bind(job.attempt_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((generation_id, attempt_state, raw, generation_state)) = row else {
        return Ok(PipelineDelivery::Dead("the pipeline attempt no longer exists".into()));
    };
    if raw.get("run").is_some() {
        // Already submitted: a replay after the record committed. The run is
        // followed by `reconcile_pipelines`, never submitted again.
        return Ok(PipelineDelivery::Done);
    }
    if raw.get("submit_started_at").is_some() {
        // Claimed but no handle recorded: the process died between the claim
        // and the record. The backend may still know the run, and submitting
        // again could run the work twice, so ask before deciding. The read is
        // outside the generation row lock: it is a remote call.
        drop(tx);
        return match pipeline.find(&job.request).await {
            Ok(Some(run)) => match record_pipeline_submitted(hub, job.attempt_id, &run).await {
                Ok(()) => Ok(PipelineDelivery::Done),
                // The run exists; only the record failed. Keep the claim, so
                // the next delivery adopts it instead of submitting again.
                Err(e) => Ok(PipelineDelivery::Retry(format!("pipeline run {} found but not recorded: {e}", run.external_id))),
            },
            Ok(None) => fail_unknown_submission(hub, generation_id, job.attempt_id).await,
            // A failed lookup proves nothing either way: the claim stays, and
            // a later delivery adopts the run the first submission started.
            Err(e) => Ok(PipelineDelivery::Retry(format!("cannot look up the claimed submission: {e}"))),
        };
    }
    if generation_state != GenerationState::Requested.as_str() || attempt_state != AttemptState::Submitted.as_str() {
        return Ok(PipelineDelivery::Done);
    }
    // Durable claim, in its own transaction before the external call.
    let claimed = sqlx::query(
        "update execution_attempts set raw = raw || jsonb_build_object('submit_started_at', now()), updated_at = now()
          where id = $1 and state = 'submitted' and not (raw ? 'submit_started_at')",
    )
    .bind(job.attempt_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    if claimed == 0 {
        return Ok(PipelineDelivery::Done);
    }
    match pipeline.submit(&job.request).await {
        Ok(run) => match record_pipeline_submitted(hub, job.attempt_id, &run).await {
            Ok(()) => Ok(PipelineDelivery::Done),
            Err(e) => Ok(PipelineDelivery::Dead(format!("pipeline run {} started but not recorded: {e}", run.external_id))),
        },
        // The request provably never left the hub: the claim was a false alarm.
        Err(PortError::Unreachable(m)) => {
            release_submission_claim(hub, job.attempt_id).await?;
            Ok(PipelineDelivery::Retry(m))
        }
        Err(PortError::Rejected { status, message }) if status >= 500 || status == 429 => {
            release_submission_claim(hub, job.attempt_id).await?;
            Ok(PipelineDelivery::Retry(format!("{status}: {message}")))
        }
        // The backend answered, or may have: never resubmit; end the request.
        Err(e) => {
            let why = match e {
                PortError::Ambiguous(_) => SUBMISSION_UNKNOWN.to_string(),
                other => other.to_string(),
            };
            fail_pipeline_attempt(hub, generation_id, job.attempt_id, &why).await?;
            Ok(PipelineDelivery::Dead(why))
        }
    }
}

/// End a claimed submission the backend has no run for. The claim stays, so
/// nothing submits it again.
async fn fail_unknown_submission(hub: &Hub, generation_id: Uuid, attempt_id: Uuid) -> DResult<PipelineDelivery> {
    let mut tx = hub.db.begin().await?;
    // Lock order is generation first, then attempts (as receipts do).
    let current = lock_generation(&mut tx, generation_id).await?;
    set_attempt_in(&mut *tx, attempt_id, AttemptState::Failed, json!({ "error": SUBMISSION_UNKNOWN })).await?;
    write_derived_state(&mut tx, generation_id, current).await?;
    audit(
        &mut tx,
        &Actor::system(),
        "generation.pipeline_submission_unknown",
        "generation",
        generation_id,
        json!({ "attempt_id": attempt_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(PipelineDelivery::Dead(SUBMISSION_UNKNOWN.into()))
}

/// Apply one authoritative pipeline status to the attempt that owns `run`. A
/// status is a guard, not a new submission: every write is fenced to the run
/// the attempt follows, under the generation row lock, and never downgrades a
/// terminal attempt.
async fn apply_pipeline_status(hub: &Hub, generation_id: Uuid, attempt_id: Uuid, run: &PipelineRun, status: PipelineStatus) -> DResult<()> {
    match status {
        PipelineStatus::Pending => set_run_state(hub, generation_id, attempt_id, run, AttemptState::Queued, json!({}), json!({})).await,
        PipelineStatus::Running { progress } => {
            set_run_state(hub, generation_id, attempt_id, run, AttemptState::Running, json!({ "progress": progress }), json!({})).await
        }
        // A run that reported a deferral ended without doing the work, so the
        // attempt fails and the operator re-requests.
        //
        // ponytail: deferral fails the attempt; the hub's admission check is the primary GPU gate. Upgrade path: resubmit under one generation-scoped submission row if deferrals become common.
        PipelineStatus::Deferred { reason } => {
            tracing::info!(%reason, "pipeline deferred the run");
            set_run_state(
                hub,
                generation_id,
                attempt_id,
                run,
                AttemptState::Failed,
                json!({ "error": format!("deferred: {reason}") }),
                json!({ "deferred": reason }),
            )
            .await?;
            let mut tx = hub.db.begin().await?;
            audit(
                &mut tx,
                &Actor::system(),
                "generation.pipeline_deferred",
                "generation",
                generation_id,
                json!({ "run": run.external_id, "reason": reason }),
            )
            .await?;
            tx.commit().await?;
            Ok(())
        }
        PipelineStatus::Succeeded { artifacts } if artifacts.is_empty() => {
            // Success without a manifest is not a generation: no output exists.
            set_run_state(
                hub,
                generation_id,
                attempt_id,
                run,
                AttemptState::Failed,
                json!({ "error": "the pipeline reported success without an artifact manifest" }),
                json!({}),
            )
            .await
        }
        PipelineStatus::Succeeded { artifacts } => match record_manifest(hub, generation_id, attempt_id, Some(run), &artifacts).await {
            Ok(()) => Ok(()),
            // A completed run whose manifest the hub cannot accept (an invalid
            // digest, a missing mime) is a failed run, not a transient read:
            // end it through the same fence instead of retrying forever.
            Err(e @ DomainError::Invalid { .. }) => {
                set_run_state(hub, generation_id, attempt_id, run, AttemptState::Failed, json!({ "error": e.to_string() }), json!({})).await
            }
            Err(e) => Err(e),
        },
        PipelineStatus::Failed { reason } => {
            set_run_state(hub, generation_id, attempt_id, run, AttemptState::Failed, json!({ "error": reason }), json!({})).await
        }
        PipelineStatus::Cancelled => {
            set_run_state(hub, generation_id, attempt_id, run, AttemptState::Cancelled, json!({}), json!({})).await
        }
    }
}

/// Write one status to the attempt, fenced to the run it was read from, under
/// the generation row lock, and re-derive the request in the same transaction.
/// Terminal attempts are never downgraded.
async fn set_run_state(
    hub: &Hub,
    generation_id: Uuid,
    attempt_id: Uuid,
    run: &PipelineRun,
    state: AttemptState,
    progress: Value,
    raw: Value,
) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    let current = lock_generation(&mut tx, generation_id).await?;
    sqlx::query(
        "update execution_attempts set state = $3, progress = progress || $4, raw = raw || $5, updated_at = now()
          where id = $1 and raw -> 'run' ->> 'external_id' = $2
            and state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown')",
    )
    .bind(attempt_id)
    .bind(&run.external_id)
    .bind(state.as_str())
    .bind(progress)
    .bind(raw)
    .execute(&mut *tx)
    .await?;
    write_derived_state(&mut tx, generation_id, current).await?;
    tx.commit().await?;
    Ok(())
}

/// Bound on one remote status read: a stalled backend delays a reconcile pass
/// by at most this much per call.
const STATUS_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Every local pipeline repair, before any remote read: a generation whose
/// attempts all ended without a refresh, and a recorded manifest whose uploads
/// never arrived. Both read durable rows only, so a slow or stalled backend
/// cannot delay them.
pub async fn reconcile_pipeline_deadlines(hub: &Hub) -> DResult<()> {
    // The single safety net for every "attempt ended, request not refreshed"
    // window (a crash between the two writes, or a path that ends an attempt
    // outside its transaction): any non-terminal pipeline generation whose
    // attempts are all terminal is re-derived here.
    let stranded: Vec<Uuid> = sqlx::query_scalar(
        "select g.id from generation_requests g join workflows w on w.id = g.workflow_id
          where w.pipeline_id is not null
            and g.state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown')
            and exists (select 1 from execution_attempts a
                         where a.generation_request_id = g.id and a.kind = 'pipeline_run')
            and not exists (select 1 from execution_attempts a
                             where a.generation_request_id = g.id and a.kind = 'pipeline_run'
                               and a.state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown'))",
    )
    .fetch_all(&hub.db)
    .await?;
    for generation_id in stranded {
        refresh_generation_state(hub, generation_id).await?;
    }
    let overdue: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "select a.id, a.generation_request_id from execution_attempts a
          where a.kind = 'pipeline_run' and a.state in ('submitted', 'queued', 'running')
            and a.raw ? 'manifest_at'
            and (a.raw ->> 'manifest_at')::bigint < extract(epoch from now())::bigint - $1
            and exists (select 1 from generation_artifacts x
                         where x.generation_request_id = a.generation_request_id and x.asset_id is null)",
    )
    .bind(ARTIFACT_UPLOAD_DEADLINE_SECS)
    .fetch_all(&hub.db)
    .await?;
    for (attempt_id, generation_id) in overdue {
        let missing: Vec<String> = sqlx::query_scalar(
            "select source from generation_artifacts where generation_request_id = $1 and asset_id is null order by source",
        )
        .bind(generation_id)
        .fetch_all(&hub.db)
        .await?;
        set_attempt_and_refresh(
            hub,
            generation_id,
            attempt_id,
            AttemptState::Failed,
            json!({ "error": "artifact uploads missing", "missing": missing }),
        )
        .await?;
    }
    Ok(())
}

/// Follow open pipeline runs. The local passes run first and every remote read
/// is bounded, so a slow backend cannot delay the deadline or the recovery
/// sweep.
pub async fn reconcile_pipelines(hub: &Hub) -> DResult<()> {
    reconcile_pipeline_deadlines(hub).await?;
    follow_open_runs(hub).await
}

async fn follow_open_runs(hub: &Hub) -> DResult<()> {
    let open: Vec<(Uuid, Uuid, String, Value)> = sqlx::query_as(&format!(
        "with picked as ({}) select p.id, g.id, w.pipeline_id, p.raw from picked p
         join generation_requests g on g.id = p.generation_request_id join workflows w on w.id = g.workflow_id",
        poll_batch("kind = 'pipeline_run' and state in ('submitted', 'queued', 'running') and raw ? 'run'")
    ))
    .fetch_all(&hub.db)
    .await?;
    for (attempt_id, generation_id, pipeline_id, raw) in open {
        let Some(pipeline) = hub.pipelines.get(&pipeline_id) else { continue };
        let Some(run) = raw.get("run").and_then(|v| serde_json::from_value::<PipelineRun>(v.clone()).ok()) else { continue };
        let status = match tokio::time::timeout(STATUS_READ_TIMEOUT, pipeline.status(&run)).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                tracing::debug!(%pipeline_id, error = %e, "pipeline status read failed");
                continue;
            }
            Err(_) => {
                tracing::warn!(%pipeline_id, run = %run.external_id, "pipeline status read timed out");
                continue;
            }
        };
        if let Err(e) = apply_pipeline_status(hub, generation_id, attempt_id, &run, status).await {
            // One run's rejected status must not stop the other runs.
            tracing::warn!(pipeline_id = %pipeline_id, run = %run.external_id, error = %e, "pipeline status apply failed");
        }
    }
    Ok(())
}

/// A dead pipeline submission is never delivered again: end its attempt in the
/// same transaction that dead-letters the outbox row, keeping the durable
/// submission claim so a run the row may already have started is never
/// submitted a second time.
pub async fn fail_dead_pipeline_submission(tx: &mut sqlx::PgConnection, attempt_id: Uuid, error: &str) -> DResult<()> {
    let generation_id: Option<Uuid> = sqlx::query_scalar("select generation_request_id from execution_attempts where id = $1")
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(generation_id) = generation_id else { return Ok(()) };
    let current = lock_generation(&mut *tx, generation_id).await?;
    set_attempt_in(&mut *tx, attempt_id, AttemptState::Failed, json!({ "error": error })).await?;
    write_derived_state(&mut *tx, generation_id, current).await?;
    audit(
        &mut *tx,
        &Actor::system(),
        "generation.pipeline_submission_dead",
        "generation",
        generation_id,
        json!({ "attempt_id": attempt_id, "error": error }),
    )
    .await?;
    Ok(())
}

/// True when the generation's terminal manifest is recorded and every manifest
/// row has its durable asset with a matching digest. A pipeline generation only
/// succeeds once this holds.
async fn manifest_complete_in(tx: &mut sqlx::PgConnection, generation_id: Uuid) -> DResult<bool> {
    let manifest: bool = sqlx::query_scalar(EXISTS_MANIFEST).bind(generation_id).fetch_one(&mut *tx).await?;
    let missing: i64 = sqlx::query_scalar(
        "select count(*) from generation_artifacts x left join assets s on s.id = x.asset_id
          where x.generation_request_id = $1 and (x.asset_id is null or s.state <> 'ready' or s.sha256 <> x.sha256)",
    )
    .bind(generation_id)
    .fetch_one(&mut *tx)
    .await?;
    Ok(manifest && missing == 0)
}

/// Why a queued generation command must not start a procedure: it already
/// started (a replay after a crash) or the request was cancelled while queued.
/// `Some(true)` = already done, `Some(false)` = drop it, `None` = send.
pub async fn dispatch_blocker(hub: &Hub, generation_id: Uuid) -> DResult<Option<(bool, String)>> {
    let (state, started): (String, bool) = sqlx::query_as(
        "select g.state, exists(select 1 from execution_attempts a where a.generation_request_id = g.id and a.kind = 'windmill_root')
         from generation_requests g where g.id = $1",
    )
    .bind(generation_id)
    .fetch_one(&hub.db)
    .await?;
    Ok(if started {
        Some((true, "procedure already started".into()))
    } else if state != GenerationState::Requested.as_str() {
        Some((false, format!("generation is {state}; not submitted")))
    } else {
        None
    })
}

fn terminal(s: AttemptState) -> bool {
    matches!(s, AttemptState::Succeeded | AttemptState::Failed | AttemptState::Cancelled | AttemptState::OutcomeUnknown)
}

/// Progress from `/ws` updates display state only.
pub async fn record_progress(hub: &Hub, ev: &ProgressEvent) -> DResult<Option<String>> {
    let (pid, running) = match ev {
        ProgressEvent::Started { prompt_id } | ProgressEvent::Executing { prompt_id, .. } | ProgressEvent::Progress { prompt_id, .. } => {
            (prompt_id, true)
        }
        ProgressEvent::NodeExecuted { prompt_id, .. } => (prompt_id, true),
        // Terminal hints: reconcile through history, do not trust the event.
        ProgressEvent::Finished { prompt_id } | ProgressEvent::Error { prompt_id, .. } | ProgressEvent::Interrupted { prompt_id } => {
            (prompt_id, false)
        }
    };
    let n = sqlx::query(
        "update execution_attempts set progress = progress || jsonb_build_object('last', $2::jsonb, 'at', now()),
           state = case when $3 and state in ('submitted', 'queued') then 'running' else state end, updated_at = now()
         where kind = 'comfy_prompt' and external_id = $1 and state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown')",
    )
    .bind(pid)
    .bind(serde_json::to_value(ev).unwrap_or_default())
    .bind(running)
    .execute(&hub.db)
    .await?
    .rows_affected();
    Ok((n > 0 && !running).then(|| pid.clone()))
}

#[derive(Debug, Clone)]
pub struct OpenPrompt {
    pub attempt_id: Uuid,
    pub generation_id: Uuid,
    pub content_id: Uuid,
    pub prompt_id: String,
    pub base_url: String,
    pub engine_id: String,
    pub workflow: Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// ComfyUI prompts still open on their engine. Pipeline runs never appear
/// here: they are followed by [`reconcile_pipelines`] through `Pipeline`, not
/// by engine history.
pub async fn open_prompts(hub: &Hub) -> DResult<Vec<OpenPrompt>> {
    type Row = (Uuid, Uuid, Uuid, String, String, String, Value, chrono::DateTime<chrono::Utc>);
    let rows: Vec<Row> = sqlx::query_as(
        "select a.id, g.id, g.content_item_id, a.external_id, e.base_url, e.id,
           jsonb_build_object('name', w.name, 'version', w.version, 'graph_sha256', w.graph_sha256), a.created_at
         from execution_attempts a join generation_requests g on g.id = a.generation_request_id
         join workflows w on w.id = g.workflow_id left join engine_instances e on e.id = w.engine_instance_id
         where a.kind = 'comfy_prompt' and a.state in ('submitted', 'queued', 'running') and e.id is not null",
    )
    .fetch_all(&hub.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(attempt_id, generation_id, content_id, prompt_id, base_url, engine_id, workflow, created_at)| OpenPrompt {
            attempt_id,
            generation_id,
            content_id,
            prompt_id,
            base_url,
            engine_id,
            workflow,
            created_at,
        })
        .collect())
}

/// Never move an attempt out of a terminal state; late receipts only add data.
const SET_ATTEMPT: &str = "update execution_attempts set state = $2, progress = progress || $3, updated_at = now()
     where id = $1 and state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown')";

async fn set_attempt(hub: &Hub, attempt: Uuid, state: AttemptState, progress: Value) -> DResult<()> {
    sqlx::query(SET_ATTEMPT).bind(attempt).bind(state.as_str()).bind(progress).execute(&hub.db).await?;
    Ok(())
}

async fn set_attempt_in(tx: &mut sqlx::PgConnection, attempt: Uuid, state: AttemptState, progress: Value) -> DResult<()> {
    sqlx::query(SET_ATTEMPT).bind(attempt).bind(state.as_str()).bind(progress).execute(tx).await?;
    Ok(())
}

/// How long a prompt may be unknown to the engine before we stop waiting for
/// the procedure to submit it.
const SUBMIT_GRACE_SECS: i64 = 120;

/// Reconcile one prompt against the engine's authoritative history.
pub async fn reconcile_prompt(hub: &Hub, p: &OpenPrompt) -> DResult<()> {
    let outcome = match hub.comfy.outcome(&p.base_url, &p.prompt_id).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(prompt_id = %p.prompt_id, error = %e, "comfy reconcile read failed");
            return Ok(());
        }
    };
    match outcome {
        PromptOutcome::Pending { running, queue_position } => {
            let state = if running { AttemptState::Running } else { AttemptState::Queued };
            set_attempt(hub, p.attempt_id, state, json!({ "queue_position": queue_position })).await?;
        }
        PromptOutcome::Succeeded { outputs } => {
            for out in &outputs {
                import_output(hub, p, out).await?;
            }
            set_attempt(hub, p.attempt_id, AttemptState::Succeeded, json!({ "outputs": outputs.len() })).await?;
        }
        PromptOutcome::Failed { message } => set_attempt(hub, p.attempt_id, AttemptState::Failed, json!({ "error": message })).await?,
        PromptOutcome::Unknown => {
            let age = (chrono::Utc::now() - p.created_at).num_seconds();
            // A command still in the outbox (e.g. waiting for GPU admission)
            // has not started submitting; its prompts cannot be overdue yet.
            let queued: bool = sqlx::query_scalar("select exists(select 1 from outbox where idempotency_key = $1 and state = 'pending')")
                .bind(format!("generation:{}", p.generation_id))
                .fetch_one(&hub.db)
                .await?;
            if age > SUBMIT_GRACE_SECS && !queued {
                // Did the procedure finish? If it failed, the prompt was never
                // submitted; if it succeeded, the engine lost track of it.
                let root: Option<String> = sqlx::query_scalar(
                    "select external_id from execution_attempts where generation_request_id = $1 and kind = 'windmill_root' order by created_at limit 1",
                )
                .bind(p.generation_id)
                .fetch_optional(&hub.db)
                .await?;
                let root_status = match root {
                    Some(r) => hub.windmill.job(&r).await.ok().map(|j| j.status),
                    None => None,
                };
                match root_status {
                    Some(JobStatus::Failed) | Some(JobStatus::Cancelled) => {
                        set_attempt(
                            hub,
                            p.attempt_id,
                            AttemptState::Failed,
                            json!({ "error": "procedure ended without submitting this prompt" }),
                        )
                        .await?
                    }
                    Some(JobStatus::Succeeded) => {
                        set_attempt(
                            hub,
                            p.attempt_id,
                            AttemptState::OutcomeUnknown,
                            json!({ "error": "engine has no record of this prompt" }),
                        )
                        .await?
                    }
                    // No confirmed procedure start at all: stop waiting silently.
                    None if age > SUBMIT_GRACE_SECS * 5 => {
                        set_attempt(
                            hub,
                            p.attempt_id,
                            AttemptState::OutcomeUnknown,
                            json!({ "error": "procedure start was never confirmed" }),
                        )
                        .await?
                    }
                    _ => {}
                }
            }
        }
    }
    refresh_generation_state(hub, p.generation_id).await
}

/// Follow open Windmill procedures and pipeline runs to their end state.
/// Display and completion only: a ComfyUI generation's state comes from its
/// prompts, never from the procedure. The local pipeline passes run first, so a
/// slow backend delays them at most one remote read each.
/// Claim the `POLL_BATCH` least recently polled attempts matching `filter`,
/// stamping `raw.polled_at` before any remote call, so with N open attempts
/// each is polled within ceil(N / POLL_BATCH) ticks even when calls time out.
/// `filter` is a fixed SQL fragment from this module, never input.
fn poll_batch(filter: &str) -> String {
    format!(
        "update execution_attempts set raw = raw || jsonb_build_object('polled_at', now())
          where id in (select id from execution_attempts where {filter}
                        order by (raw->>'polled_at')::timestamptz nulls first limit {POLL_BATCH}
                        for update skip locked)
          returning id, generation_request_id, external_id, raw"
    )
}

const POLL_BATCH: i64 = 20;

pub async fn reconcile_procedures(hub: &Hub) -> DResult<()> {
    let open: Vec<(Uuid, String)> = sqlx::query_as(&format!(
        "with picked as ({}) select id, external_id from picked",
        poll_batch("kind = 'windmill_root' and state in ('submitted', 'queued', 'running')")
    ))
    .fetch_all(&hub.db)
    .await?;
    for (id, job) in open {
        let read = match tokio::time::timeout(STATUS_READ_TIMEOUT, hub.windmill.job(&job)).await {
            Ok(read) => read,
            Err(_) => {
                tracing::warn!(%job, "procedure status read timed out");
                continue;
            }
        };
        let state = match read {
            Ok(view) => match view.status {
                JobStatus::Queued => AttemptState::Queued,
                JobStatus::Running => AttemptState::Running,
                JobStatus::Succeeded => AttemptState::Succeeded,
                JobStatus::Failed => AttemptState::Failed,
                JobStatus::Cancelled => AttemptState::Cancelled,
            },
            Err(e) => {
                tracing::debug!(%job, error = %e, "procedure status read failed");
                continue;
            }
        };
        set_attempt(hub, id, state, json!({})).await?;
    }
    follow_open_runs(hub).await
}

/// Copy one engine output into durable storage. Idempotent per output.
async fn import_output(hub: &Hub, p: &OpenPrompt, out: &OutputFile) -> DResult<()> {
    let source_key = format!("{}:{}:{}:{}:{}:{}", p.engine_id, p.prompt_id, out.node_id, out.kind, out.subfolder, out.filename);
    let state: Option<String> =
        sqlx::query_scalar("select state from assets where source_key = $1").bind(&source_key).fetch_optional(&hub.db).await?;
    if state.as_deref() == Some("ready") {
        return Ok(());
    }
    let provenance = json!({
        "engine_instance_id": p.engine_id, "engine_base_url": p.base_url, "prompt_id": p.prompt_id,
        "node_id": out.node_id, "filename": out.filename, "subfolder": out.subfolder, "type": out.kind,
        "workflow": p.workflow, "generation_request_id": p.generation_id,
    });
    let mime = mime_for(&out.filename);
    let asset_id: Uuid = sqlx::query_scalar(
        "insert into assets (id, content_item_id, generation_request_id, sha256, storage_key, mime, bytes, state, provenance, source_key)
         values ($1, $2, $3, '', '', $4, 0, 'importing', $5, $6)
         on conflict (source_key) do update set provenance = excluded.provenance returning id",
    )
    .bind(Uuid::new_v4())
    .bind(p.content_id)
    .bind(p.generation_id)
    .bind(mime)
    .bind(&provenance)
    .bind(&source_key)
    .fetch_one(&hub.db)
    .await?;
    let bytes = match hub.comfy.fetch(&p.base_url, out).await {
        Ok(b) => b,
        Err(e) => {
            sqlx::query("update assets set state = 'failed', provenance = provenance || $2 where id = $1")
                .bind(asset_id)
                .bind(json!({ "import_error": e.to_string() }))
                .execute(&hub.db)
                .await?;
            return Err(DomainError::Unavailable { code: "asset_fetch_failed", message: e.to_string() });
        }
    };
    let sha = hash::sha256_hex(&bytes);
    let key = hub.assets.put(&sha, &bytes).await.map_err(|e| DomainError::Internal(format!("asset store: {e}")))?;
    // Only a finalized, checksummed object makes the asset usable.
    sqlx::query("update assets set state = 'ready', sha256 = $2, storage_key = $3, bytes = $4 where id = $1")
        .bind(asset_id)
        .bind(&sha)
        .bind(&key)
        .bind(bytes.len() as i64)
        .execute(&hub.db)
        .await?;
    let mut tx = hub.db.begin().await?;
    audit(
        &mut tx,
        &Actor::system(),
        "asset.imported",
        "content",
        p.content_id,
        json!({ "asset_id": asset_id, "sha256": sha, "prompt_id": p.prompt_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

fn mime_for(filename: &str) -> &'static str {
    match filename.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
}

/// True for the states a generation never leaves. A refresh that reaches one
/// of them is a no-op, however stale the attempts it read.
fn generation_terminal(s: GenerationState) -> bool {
    matches!(s, GenerationState::Succeeded | GenerationState::Failed | GenerationState::Cancelled | GenerationState::OutcomeUnknown)
}

/// Read the generation's state under its row lock. Every write that ends an
/// attempt takes this lock first, then the attempt, so a refresh can never
/// derive from attempts read before a cancel committed.
async fn lock_generation(tx: &mut sqlx::PgConnection, generation_id: Uuid) -> DResult<GenerationState> {
    let state: Option<String> = sqlx::query_scalar("select state from generation_requests where id = $1 for update")
        .bind(generation_id)
        .fetch_optional(&mut *tx)
        .await?;
    let state = state.ok_or(DomainError::NotFound("generation request"))?;
    Ok(GenerationState::parse(&state).unwrap_or(GenerationState::Requested))
}

/// Derive the request state from its attempts and write it. The caller holds
/// the generation row lock and read `current` under it; the write and the
/// caller's attempt write commit together. A pipeline generation is only
/// succeeded once its complete manifest is durable.
async fn write_derived_state(tx: &mut sqlx::PgConnection, generation_id: Uuid, current: GenerationState) -> DResult<()> {
    if generation_terminal(current) {
        // A terminal generation is final: never let a late attempt write or a
        // stale read revive it.
        return Ok(());
    }
    let pipeline: Option<String> =
        sqlx::query_scalar("select w.pipeline_id from generation_requests g join workflows w on w.id = g.workflow_id where g.id = $1")
            .bind(generation_id)
            .fetch_one(&mut *tx)
            .await?;
    let kind = if pipeline.is_some() { AttemptKind::PipelineRun } else { AttemptKind::ComfyPrompt };
    let states: Vec<String> = sqlx::query_scalar("select state from execution_attempts where generation_request_id = $1 and kind = $2")
        .bind(generation_id)
        .bind(kind.as_str())
        .fetch_all(&mut *tx)
        .await?;
    let states: Vec<AttemptState> = states.iter().filter_map(|s| AttemptState::parse(s)).collect();
    let mut next = derive_generation_state(current, &states);
    if next == GenerationState::Succeeded && pipeline.is_some() && !manifest_complete_in(tx, generation_id).await? {
        // The manifest is recorded, but not every output has been uploaded.
        next = GenerationState::Running;
    }
    let changed = sqlx::query("update generation_requests set state = $2, updated_at = now() where id = $1 and state <> $2")
        .bind(generation_id)
        .bind(next.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if changed > 0 && matches!(next, GenerationState::Succeeded) {
        let content: Uuid = sqlx::query_scalar("select content_item_id from generation_requests where id = $1")
            .bind(generation_id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("update content_items set state = 'selecting', updated_at = now() where id = $1 and state = 'producing'")
            .bind(content)
            .execute(&mut *tx)
            .await?;
    }
    if changed > 0 {
        audit(tx, &Actor::system(), "generation.state", "generation", generation_id, json!({ "state": next })).await?;
    }
    Ok(())
}

/// Re-derive one generation in its own transaction.
pub async fn refresh_generation_state(hub: &Hub, generation_id: Uuid) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    let current = lock_generation(&mut tx, generation_id).await?;
    write_derived_state(&mut tx, generation_id, current).await?;
    tx.commit().await?;
    Ok(())
}

/// End one attempt and re-derive its request in the same transaction, so a
/// crash between the two writes cannot strand the generation.
async fn set_attempt_and_refresh(hub: &Hub, generation_id: Uuid, attempt_id: Uuid, state: AttemptState, progress: Value) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    // Lock order is generation first, then attempts (as receipts do).
    let current = lock_generation(&mut tx, generation_id).await?;
    set_attempt_in(&mut tx, attempt_id, state, progress).await?;
    write_derived_state(&mut tx, generation_id, current).await?;
    tx.commit().await?;
    Ok(())
}

pub fn derive_generation_state(current: GenerationState, attempts: &[AttemptState]) -> GenerationState {
    if attempts.is_empty() {
        return current;
    }
    if !attempts.iter().all(|s| terminal(*s)) {
        let started = attempts.iter().any(|s| matches!(s, AttemptState::Running | AttemptState::Queued | AttemptState::Succeeded));
        return match current {
            GenerationState::Cancelling => GenerationState::Cancelling,
            _ if started => GenerationState::Running,
            other => other,
        };
    }
    if attempts.contains(&AttemptState::Succeeded) {
        GenerationState::Succeeded
    } else if attempts.contains(&AttemptState::OutcomeUnknown) {
        GenerationState::OutcomeUnknown
    } else if attempts.iter().all(|s| *s == AttemptState::Cancelled) {
        GenerationState::Cancelled
    } else {
        GenerationState::Failed
    }
}

/// Cancel only this request's engine work. Stopping the Windmill job alone
/// would not stop ComfyUI, so both are addressed; a pipeline run is cancelled
/// through its own `Pipeline`.
pub async fn cancel(hub: &Hub, actor: &Actor, generation_id: Uuid) -> DResult<CancelOut> {
    actor.require(OPERATOR_OR_AGENT)?;
    // Drop the command if it is still queued. This waits for the worker's row
    // lock, so a delivery in progress finishes (and records its Windmill job)
    // before the attempts are read below.
    sqlx::query("update outbox set state = 'dead', last_error = 'generation cancelled' where idempotency_key = $1 and state = 'pending'")
        .bind(format!("generation:{generation_id}"))
        .execute(&hub.db)
        .await?;
    let generation = get_generation(hub, generation_id).await?;
    let (pipeline, engine_url): (Option<String>, Option<String>) = sqlx::query_as(
        "select w.pipeline_id, e.base_url from generation_requests g join workflows w on w.id = g.workflow_id
         left join engine_instances e on e.id = w.engine_instance_id where g.id = $1",
    )
    .bind(generation_id)
    .fetch_one(&hub.db)
    .await?;
    if let Some(pipeline_id) = pipeline {
        return cancel_pipeline(hub, actor, generation, &pipeline_id).await;
    }
    let engine_url = engine_url.ok_or_else(|| invalid("workflow_without_engine", "this ComfyUI workflow has no engine registered"))?;
    sqlx::query("update generation_requests set state = 'cancelling', updated_at = now() where id = $1 and state in ('requested', 'dispatched', 'running')")
        .bind(generation_id)
        .execute(&hub.db)
        .await?;
    let mut actions = vec![];
    // Stop the procedure first so it cannot submit more prompts, then the prompts.
    let mut attempts = generation.attempts.clone();
    attempts.sort_by_key(|a| a.kind == AttemptKind::ComfyPrompt);
    for a in &attempts {
        if terminal(a.state) {
            continue;
        }
        match a.kind {
            AttemptKind::ComfyPrompt => match hub.comfy.cancel(&engine_url, &a.external_id).await {
                Ok(CancelOutcome::Dequeued) | Ok(CancelOutcome::Interrupted) => {
                    set_attempt(hub, a.id, AttemptState::Cancelled, json!({ "cancelled_by": actor.name() })).await?;
                    actions.push(format!("{}: cancelled", a.external_id));
                }
                Ok(CancelOutcome::AlreadyFinished) => actions.push(format!("{}: already finished; reconciling", a.external_id)),
                Ok(CancelOutcome::Unsupported(why)) => actions.push(format!("{}: not cancelled ({why})", a.external_id)),
                // The engine never saw it (not yet submitted): mark cancelled so
                // the receipt cannot revive it, and stop the procedure below.
                Err(crate::ports::PortError::NotFound) => {
                    set_attempt(hub, a.id, AttemptState::Cancelled, json!({ "cancelled_by": actor.name(), "note": "not on engine" }))
                        .await?;
                    actions.push(format!("{}: not on engine; cancelled", a.external_id));
                }
                Err(e) => actions.push(format!("{}: cancel failed ({e})", a.external_id)),
            },
            AttemptKind::WindmillRoot | AttemptKind::WindmillChild => match hub.windmill.cancel_job(&a.external_id).await {
                Ok(()) => {
                    set_attempt(hub, a.id, AttemptState::Cancelled, json!({})).await?;
                    actions.push(format!("windmill {}: cancelled", a.external_id));
                }
                Err(e) => actions.push(format!("windmill {}: {e}", a.external_id)),
            },
            AttemptKind::PipelineRun => {}
        }
    }
    refresh_generation_state(hub, generation_id).await?;
    let mut tx = hub.db.begin().await?;
    audit(
        &mut tx,
        actor,
        "generation.cancel",
        "content",
        generation.content_item_id,
        json!({ "generation_request_id": generation_id, "actions": actions }),
    )
    .await?;
    tx.commit().await?;
    Ok(CancelOut { state: get_generation(hub, generation_id).await?.state, actions })
}

/// Cancel a pipeline generation through the pipeline that runs it. The queued
/// command is already dropped. Cancelling and a delivery serialize on the
/// generation row lock: either the run handle is recorded and cancelled here,
/// or the delivery sees the cancelled attempt and stops the run it submitted.
async fn cancel_pipeline(hub: &Hub, actor: &Actor, generation: GenerationRequest, pipeline_id: &str) -> DResult<CancelOut> {
    let mut tx = hub.db.begin().await?;
    sqlx::query("select id from generation_requests where id = $1 for update").bind(generation.id).execute(&mut *tx).await?;
    sqlx::query("update generation_requests set state = 'cancelling', updated_at = now() where id = $1 and state in ('requested', 'dispatched', 'running')")
        .bind(generation.id)
        .execute(&mut *tx)
        .await?;
    let open: Vec<(Uuid, Value)> = sqlx::query_as(
        "select id, raw from execution_attempts where generation_request_id = $1 and kind = 'pipeline_run'
           and state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown')",
    )
    .bind(generation.id)
    .fetch_all(&mut *tx)
    .await?;
    let mut actions = vec![];
    let mut runs = vec![];
    for (attempt_id, raw) in open {
        match raw.get("run").and_then(|v| serde_json::from_value::<PipelineRun>(v.clone()).ok()) {
            Some(run) => runs.push((attempt_id, run, raw.get("manifest_at").is_some())),
            // Never submitted, and the delivery's claim/serialization point is
            // this same row lock, so no submission can start after this
            // commits.
            None => {
                set_attempt_in(
                    &mut *tx,
                    attempt_id,
                    AttemptState::Cancelled,
                    json!({ "cancelled_by": actor.name(), "note": "never submitted" }),
                )
                .await?;
                actions.push("pipeline_run: never submitted; cancelled".into());
            }
        }
    }
    tx.commit().await?;
    for (attempt_id, run, manifest_recorded) in runs {
        match hub.pipelines.get(pipeline_id) {
            Some(pipeline) => match pipeline.cancel(&run).await {
                Ok(CancelOutcome::Dequeued) | Ok(CancelOutcome::Interrupted) => {
                    set_attempt(hub, attempt_id, AttemptState::Cancelled, json!({ "cancelled_by": actor.name() })).await?;
                    actions.push(format!("{}: cancelled", run.external_id));
                }
                // The run finished, but a recorded manifest still waits for its
                // uploads: a cancellation ends it instead of waiting.
                Ok(CancelOutcome::AlreadyFinished) if manifest_recorded => {
                    set_attempt(
                        hub,
                        attempt_id,
                        AttemptState::Cancelled,
                        json!({ "cancelled_by": actor.name(), "note": "manifest recorded; uploads abandoned" }),
                    )
                    .await?;
                    actions.push(format!("{}: already finished; uploads abandoned", run.external_id));
                }
                Ok(CancelOutcome::AlreadyFinished) => actions.push(format!("{}: already finished; reconciling", run.external_id)),
                Ok(CancelOutcome::Unsupported(why)) => actions.push(format!("{}: not cancelled ({why})", run.external_id)),
                Err(PortError::NotFound) => {
                    set_attempt(
                        hub,
                        attempt_id,
                        AttemptState::Cancelled,
                        json!({ "cancelled_by": actor.name(), "note": "not on pipeline" }),
                    )
                    .await?;
                    actions.push(format!("{}: not on pipeline; cancelled", run.external_id));
                }
                Err(e) => actions.push(format!("{}: cancel failed ({e})", run.external_id)),
            },
            None => actions.push(format!("{}: no pipeline registered for {pipeline_id}", run.external_id)),
        }
    }
    refresh_generation_state(hub, generation.id).await?;
    let mut tx = hub.db.begin().await?;
    audit(
        &mut tx,
        actor,
        "generation.cancel",
        "content",
        generation.content_item_id,
        json!({ "generation_request_id": generation.id, "pipeline_id": pipeline_id, "actions": actions }),
    )
    .await?;
    tx.commit().await?;
    Ok(CancelOut { state: get_generation(hub, generation.id).await?.state, actions })
}

// ── GPU admission ───────────────────────────────────────────────────────────

/// Per-engine admission policy, from `engine_instances.capabilities.admission`.
/// Absent policy admits immediately.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AdmissionPolicy {
    #[serde(default = "one")]
    pub max_in_flight: i64,
    #[serde(default = "yes")]
    pub defer_on_unknown_work: bool,
    #[serde(default)]
    pub defer_while_llm_loaded: bool,
    #[serde(default)]
    pub llm_running_url: Option<String>,
}

fn one() -> i64 {
    1
}
fn yes() -> bool {
    true
}

#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    Admit,
    Defer(String),
}

/// Decide whether a generation may start on its engine now. A pipeline
/// workflow names its engine only as the admission target; without one it
/// admits immediately (CPU work). Any read that fails defers (fail closed): an
/// unknown job is not a free device.
pub async fn admission(hub: &Hub, generation_id: Uuid) -> DResult<Admission> {
    let row: Option<(Option<String>, Option<String>, Option<Value>)> = sqlx::query_as(
        "select w.pipeline_id, e.id, e.capabilities from generation_requests g join workflows w on w.id = g.workflow_id
         left join engine_instances e on e.id = w.engine_instance_id where g.id = $1",
    )
    .bind(generation_id)
    .fetch_optional(&hub.db)
    .await?;
    let (pipeline_id, engine, caps) = row.ok_or(DomainError::NotFound("generation request"))?;
    let (Some(engine_id), Some(caps)) = (engine, caps) else {
        // A pipeline without an admission target is admitted immediately.
        return Ok(Admission::Admit);
    };
    let Some(policy) = caps.get("admission") else { return Ok(Admission::Admit) };
    let policy: AdmissionPolicy = serde_json::from_value(policy.clone()).map_err(|e| invalid("invalid_admission", e.to_string()))?;

    let ours_in_flight: i64 = sqlx::query_scalar(
        "select count(*) from generation_requests g join workflows w on w.id = g.workflow_id
         where w.engine_instance_id = $1 and g.id <> $2 and g.state in ('dispatched', 'running', 'cancelling')",
    )
    .bind(&engine_id)
    .bind(generation_id)
    .fetch_one(&hub.db)
    .await?;
    if ours_in_flight >= policy.max_in_flight {
        return Ok(Admission::Defer(format!("{engine_id}: {ours_in_flight} hub generation(s) in flight (limit {})", policy.max_in_flight)));
    }
    // A pipeline target's base URL is not a ComfyUI queue; only the hub's own
    // ComfyUI generations are checked for foreign work.
    if policy.defer_on_unknown_work && pipeline_id.is_none() {
        let base_url: String =
            sqlx::query_scalar("select base_url from engine_instances where id = $1").bind(&engine_id).fetch_one(&hub.db).await?;
        let load = match hub.comfy.load(&base_url).await {
            Ok(l) => l,
            Err(e) => return Ok(Admission::Defer(format!("{engine_id}: cannot read engine queue ({e})"))),
        };
        let ours: Vec<String> =
            sqlx::query_scalar("select external_id from execution_attempts where kind = 'comfy_prompt'").fetch_all(&hub.db).await?;
        let unknown = load.running.iter().chain(&load.pending).filter(|p| !ours.contains(p)).count();
        if unknown > 0 {
            return Ok(Admission::Defer(format!("{engine_id}: {unknown} job(s) the hub did not submit are queued or running")));
        }
    }
    if policy.defer_while_llm_loaded
        && let Some(url) = &policy.llm_running_url
    {
        match hub.gpu_peer.busy(url).await {
            Ok(false) => {}
            Ok(true) => return Ok(Admission::Defer(format!("{engine_id}: shared GPU has an LLM loaded ({url})"))),
            Err(e) => return Ok(Admission::Defer(format!("{engine_id}: cannot read GPU peer {url} ({e})"))),
        }
    }
    Ok(Admission::Admit)
}

// ── Assets ──────────────────────────────────────────────────────────────────

const ASSET_COLS: &str =
    "id, content_item_id, generation_request_id, sha256, mime, bytes, state, selected, alt_text, provenance, created_at";

pub async fn assets_for_content(hub: &Hub, content_id: Uuid) -> DResult<Vec<Asset>> {
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {ASSET_COLS} from assets where content_item_id = $1 order by created_at) t"
    ))
    .bind(content_id)
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

pub async fn all_assets(hub: &Hub, actor: &Actor) -> DResult<Vec<Asset>> {
    actor.require(crate::ANY_READER)?;
    let rows: Vec<Value> = sqlx::query_scalar(&format!(
        "select to_jsonb(t) from (select {ASSET_COLS} from assets where state = 'ready' order by created_at desc limit 500) t"
    ))
    .fetch_all(&hub.db)
    .await?;
    decode(rows)
}

pub async fn select_asset(hub: &Hub, actor: &Actor, asset_id: Uuid, input: SelectAssetIn) -> DResult<Asset> {
    actor.require(OPERATOR_OR_AGENT)?;
    let mut tx = hub.db.begin().await?;
    let row: Option<Value> = sqlx::query_scalar(&format!(
        "update assets set selected = $2, alt_text = coalesce($3, alt_text) where id = $1 and state = 'ready'
         returning to_jsonb((select t from (select {ASSET_COLS}) t))"
    ))
    .bind(asset_id)
    .bind(input.selected)
    .bind(&input.alt_text)
    .fetch_optional(&mut *tx)
    .await?;
    let asset: Asset = decode_one(row.ok_or(DomainError::NotFound("ready asset"))?)?;
    audit(
        &mut tx,
        actor,
        if input.selected { "asset.selected" } else { "asset.unselected" },
        "content",
        asset.content_item_id,
        json!({ "asset_id": asset_id }),
    )
    .await?;
    tx.commit().await?;
    Ok(asset)
}

/// Asset bytes are the one read a `Procedure` may make: a worker downloads its
/// stage inputs by asset id and verifies each sha256 itself. Nothing else
/// widens (listings, content, releases stay reader-only).
const ASSET_CONTENT_READERS: &[Role] = &[Role::Operator, Role::Agent, Role::Procedure];

pub async fn asset_content(hub: &Hub, actor: &Actor, asset_id: Uuid) -> DResult<(String, Vec<u8>)> {
    actor.require(ASSET_CONTENT_READERS)?;
    let row: Option<(String, String, String)> =
        sqlx::query_as("select storage_key, mime, sha256 from assets where id = $1 and state = 'ready'")
            .bind(asset_id)
            .fetch_optional(&hub.db)
            .await?;
    let (key, mime, sha) = row.ok_or(DomainError::NotFound("ready asset"))?;
    let bytes = hub.assets.get(&key).await.map_err(|e| DomainError::Internal(format!("asset store: {e}")))?;
    if hash::sha256_hex(&bytes) != sha {
        return Err(DomainError::Internal("asset checksum mismatch".into()));
    }
    Ok((mime, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_graph_applies_inputs_and_varies_seed() {
        let graph = json!({"3": {"inputs": {"seed": 0, "steps": 20}}, "6": {"inputs": {"text": ""}}});
        let bindings = json!({"prompt": {"node": "6", "input": "text"}, "seed": {"node": "3", "input": "seed"}, "steps": {"node": "3", "input": "steps", "default": 20}});
        let a = bind_graph(&graph, &bindings, &json!({"prompt": "a cat"}), 0, 100).unwrap();
        let b = bind_graph(&graph, &bindings, &json!({"prompt": "a cat"}), 1, 100).unwrap();
        assert_eq!(a["6"]["inputs"]["text"], "a cat");
        assert_ne!(a["3"]["inputs"]["seed"], b["3"]["inputs"]["seed"]);
        assert!(bind_graph(&graph, &bindings, &json!({"nope": 1, "prompt": "x"}), 0, 0).is_err());
        assert!(bind_graph(&graph, &bindings, &json!({}), 0, 0).is_err(), "prompt has no default");
    }

    #[test]
    fn generation_state_never_succeeds_while_work_is_open() {
        use AttemptState::*;
        assert_eq!(derive_generation_state(GenerationState::Dispatched, &[Succeeded, Running]), GenerationState::Running);
        assert_eq!(derive_generation_state(GenerationState::Running, &[Succeeded, Failed]), GenerationState::Succeeded);
        assert_eq!(derive_generation_state(GenerationState::Running, &[Failed, Failed]), GenerationState::Failed);
        assert_eq!(derive_generation_state(GenerationState::Cancelling, &[Cancelled, Running]), GenerationState::Cancelling);
        assert_eq!(derive_generation_state(GenerationState::Cancelling, &[Cancelled, Cancelled]), GenerationState::Cancelled);
        assert_eq!(derive_generation_state(GenerationState::Running, &[OutcomeUnknown, Failed]), GenerationState::OutcomeUnknown);
    }
}
