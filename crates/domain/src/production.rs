//! Production: registered ComfyUI workflows, versioned generation requests,
//! external receipts, reconciliation and durable asset import.

use contracts::{
    Asset, AttemptKind, AttemptState, CancelOut, ContentState, GenerateIn, GenerationRequest, GenerationState, ReceiptIn, Role,
    SelectAssetIn, Workflow,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ports::{CancelOutcome, GenerationJob, JobStatus, OutputFile, ProgressEvent, PromptOutcome};
use crate::{Actor, DResult, DomainError, Hub, OPERATOR_OR_AGENT, audit, conflict, decode, decode_one, hash, invalid, outbox};

/// Version of the `GenerationJob` payload sent to the Windmill procedure.
pub const GENERATION_REQUEST_VERSION: i32 = 1;

/// A registry entry (`registry/comfyui/*.json`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowSpec {
    pub name: String,
    pub version: i32,
    pub engine_instance_id: String,
    pub windmill_path: String,
    pub bindings: Value,
    pub graph: Value,
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
/// same name/version with a different graph is refused.
pub async fn register_workflow(hub: &Hub, spec: &WorkflowSpec) -> DResult<Uuid> {
    let graph_sha = hash::sha256_hex(hash::canonical_json(&spec.graph).as_bytes());
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
        "insert into workflows (id, name, version, engine_instance_id, api_graph, graph_sha256, bindings, windmill_path)
         values ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(&spec.name)
    .bind(spec.version)
    .bind(&spec.engine_instance_id)
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

pub async fn generate(hub: &Hub, actor: &Actor, content_id: Uuid, input: GenerateIn) -> DResult<GenerationRequest> {
    actor.require(OPERATOR_OR_AGENT)?;
    if !(1..=8).contains(&input.variants) {
        return Err(invalid("invalid_variants", "variants must be 1..=8"));
    }
    let wf: Option<(Value, Value, String, String, String)> = sqlx::query_as(
        "select w.api_graph, w.bindings, w.windmill_path, w.engine_instance_id, e.base_url
         from workflows w join engine_instances e on e.id = w.engine_instance_id where w.id = $1",
    )
    .bind(input.workflow_id)
    .fetch_optional(&hub.db)
    .await?;
    let (graph, bindings, windmill_path, engine_id, engine_url) = wf.ok_or(DomainError::NotFound("workflow"))?;
    let id = Uuid::new_v4();
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
    std::env::var("HUB_COMFY_CLIENT_ID").unwrap_or_else(|_| "infernix-hub".into())
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

/// Receipt posted by the Windmill procedure (or recorded by the worker).
pub async fn record_receipt(hub: &Hub, actor: &Actor, generation_id: Uuid, r: ReceiptIn) -> DResult<GenerationRequest> {
    actor.require(&[Role::Procedure])?;
    if r.external_id.trim().is_empty() {
        return Err(invalid("invalid_receipt", "external_id is required"));
    }
    let mut tx = hub.db.begin().await?;
    let exists: Option<Uuid> = sqlx::query_scalar("select id from generation_requests where id = $1 for update")
        .bind(generation_id)
        .fetch_optional(&mut *tx)
        .await?;
    exists.ok_or(DomainError::NotFound("generation request"))?;
    let parent: Option<Uuid> = match &r.parent_external_id {
        Some(p) => {
            sqlx::query_scalar(
                "select id from execution_attempts where generation_request_id = $1 and external_id = $2 and kind <> 'comfy_prompt'",
            )
            .bind(generation_id)
            .bind(p)
            .fetch_optional(&mut *tx)
            .await?
        }
        None => None,
    };
    let (owner, state): (Uuid, String) = sqlx::query_as(
        "insert into execution_attempts (id, generation_request_id, parent_id, kind, external_id, engine_instance_id, state, raw)
         values ($1, $2, $3, $4, $5, $6, 'submitted', $7)
         on conflict (kind, external_id) do update
           set parent_id = coalesce(excluded.parent_id, execution_attempts.parent_id),
               engine_instance_id = coalesce(excluded.engine_instance_id, execution_attempts.engine_instance_id),
               raw = excluded.raw, updated_at = now()
         returning generation_request_id, state",
    )
    .bind(Uuid::new_v4())
    .bind(generation_id)
    .bind(parent)
    .bind(r.kind.as_str())
    .bind(&r.external_id)
    .bind(&r.engine_instance_id)
    .bind(&r.raw)
    .fetch_one(&mut *tx)
    .await?;
    if owner != generation_id {
        return Err(conflict("receipt_owner_mismatch", "external id belongs to a different generation request"));
    }
    sqlx::query(
        "update generation_requests set state = 'running', updated_at = now() where id = $1 and state in ('requested', 'dispatched')",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "generation.receipt", "generation", generation_id, json!({ "kind": r.kind, "external_id": r.external_id }))
        .await?;
    tx.commit().await?;
    // The operator cancelled before the procedure submitted: stop the work
    // that just reached the engine instead of letting it run orphaned.
    if r.kind == AttemptKind::ComfyPrompt && state == AttemptState::Cancelled.as_str() {
        let url: Option<String> = sqlx::query_scalar(
            "select e.base_url from generation_requests g join workflows w on w.id = g.workflow_id
             join engine_instances e on e.id = w.engine_instance_id where g.id = $1",
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

/// Record the Windmill root job id once the worker has started the procedure.
pub async fn record_dispatched(hub: &Hub, generation_id: Uuid, windmill_job_id: &str) -> DResult<()> {
    let mut tx = hub.db.begin().await?;
    sqlx::query(
        "insert into execution_attempts (id, generation_request_id, kind, external_id, state) values ($1, $2, 'windmill_root', $3, 'submitted')
         on conflict (kind, external_id) do nothing",
    )
    .bind(Uuid::new_v4())
    .bind(generation_id)
    .bind(windmill_job_id)
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

pub async fn open_prompts(hub: &Hub) -> DResult<Vec<OpenPrompt>> {
    type Row = (Uuid, Uuid, Uuid, String, String, String, Value, chrono::DateTime<chrono::Utc>);
    let rows: Vec<Row> = sqlx::query_as(
        "select a.id, g.id, g.content_item_id, a.external_id, e.base_url, e.id,
           jsonb_build_object('name', w.name, 'version', w.version, 'graph_sha256', w.graph_sha256), a.created_at
         from execution_attempts a join generation_requests g on g.id = a.generation_request_id
         join workflows w on w.id = g.workflow_id join engine_instances e on e.id = w.engine_instance_id
         where a.kind = 'comfy_prompt' and a.state in ('submitted', 'queued', 'running')",
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

async fn set_attempt(hub: &Hub, attempt: Uuid, state: AttemptState, progress: Value) -> DResult<()> {
    sqlx::query(
        "update execution_attempts set state = $2, progress = progress || $3, updated_at = now()
         where id = $1 and state not in ('succeeded', 'failed', 'cancelled', 'outcome_unknown')",
    )
    .bind(attempt)
    .bind(state.as_str())
    .bind(progress)
    .execute(&hub.db)
    .await?;
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

/// Follow open Windmill procedures to their end state. Display only: the
/// generation's state comes from its prompts, never from the procedure.
pub async fn reconcile_procedures(hub: &Hub) -> DResult<()> {
    let open: Vec<(Uuid, String)> = sqlx::query_as(
        "select id, external_id from execution_attempts where kind = 'windmill_root' and state in ('submitted', 'queued', 'running')",
    )
    .fetch_all(&hub.db)
    .await?;
    for (id, job) in open {
        let state = match hub.windmill.job(&job).await {
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
    Ok(())
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

/// Derive the request state from its engine attempts.
pub async fn refresh_generation_state(hub: &Hub, generation_id: Uuid) -> DResult<()> {
    let states: Vec<String> =
        sqlx::query_scalar("select state from execution_attempts where generation_request_id = $1 and kind = 'comfy_prompt'")
            .bind(generation_id)
            .fetch_all(&hub.db)
            .await?;
    let states: Vec<AttemptState> = states.iter().filter_map(|s| AttemptState::parse(s)).collect();
    let current: String =
        sqlx::query_scalar("select state from generation_requests where id = $1").bind(generation_id).fetch_one(&hub.db).await?;
    let next = derive_generation_state(GenerationState::parse(&current).unwrap_or(GenerationState::Requested), &states);
    let mut tx = hub.db.begin().await?;
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
        audit(&mut tx, &Actor::system(), "generation.state", "generation", generation_id, json!({ "state": next })).await?;
    }
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

/// Cancel only this request's engine work, prompt by prompt. Stopping the
/// Windmill job alone would not stop ComfyUI, so both are addressed.
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
    let engine_url: String = sqlx::query_scalar(
        "select e.base_url from generation_requests g join workflows w on w.id = g.workflow_id
         join engine_instances e on e.id = w.engine_instance_id where g.id = $1",
    )
    .bind(generation_id)
    .fetch_one(&hub.db)
    .await?;
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

/// Decide whether a generation may start on its engine now. Any read that
/// fails defers (fail closed): an unknown job is not a free device.
pub async fn admission(hub: &Hub, generation_id: Uuid) -> DResult<Admission> {
    let row: Option<(String, String, Value)> = sqlx::query_as(
        "select e.id, e.base_url, e.capabilities from generation_requests g join workflows w on w.id = g.workflow_id
         join engine_instances e on e.id = w.engine_instance_id where g.id = $1",
    )
    .bind(generation_id)
    .fetch_optional(&hub.db)
    .await?;
    let (engine_id, base_url, caps) = row.ok_or(DomainError::NotFound("generation request"))?;
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
    if policy.defer_on_unknown_work {
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

pub async fn asset_content(hub: &Hub, actor: &Actor, asset_id: Uuid) -> DResult<(String, Vec<u8>)> {
    actor.require(crate::ANY_READER)?;
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
