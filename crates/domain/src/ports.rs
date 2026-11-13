//! Ports to the systems the hub does not own. Each has a live adapter and a
//! fixture adapter in `hub-integrations`; the domain only sees these traits.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, thiserror::Error)]
pub enum PortError {
    /// The request never reached the remote side; safe to retry.
    #[error("unreachable: {0}")]
    Unreachable(String),
    /// The request may have been applied remotely (timeout/reset after send).
    /// Never resubmit on this; reconcile first.
    #[error("outcome unknown: {0}")]
    Ambiguous(String),
    /// The remote side answered and refused.
    #[error("rejected ({status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("not found")]
    NotFound,
}

pub type PortResult<T> = Result<T, PortError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForeignRef {
    pub id: String,
    pub url: Option<String>,
}

// ── Paperclip ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRequest {
    pub company_id: String,
    pub title: String,
    pub description: String,
    /// Stable hub key so a retried create links the same issue.
    pub origin_key: String,
    /// Paperclip project (studio company) that owns the issue.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Paperclip agent assigned to start the work (the Showrunner).
    #[serde(default)]
    pub assignee_agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub company_id: String,
    pub title: String,
    pub summary: String,
    pub issue_id: Option<String>,
    /// The exact release revision the approval authorizes.
    pub release_revision_id: String,
    pub payload_hash: String,
    pub actions: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    RevisionRequested,
    Cancelled,
}

/// Authoritative read of a native approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalView {
    pub id: String,
    pub status: ApprovalStatus,
    /// The human board user who decided, as Paperclip reports it.
    pub decided_by_user_id: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    /// Hash echoed from the approval's own payload.
    pub payload_hash: Option<String>,
    pub release_revision_id: Option<String>,
    pub raw: Value,
}

#[async_trait]
pub trait Paperclip: Send + Sync {
    async fn create_issue(&self, req: &IssueRequest) -> PortResult<ForeignRef>;
    async fn create_approval(&self, req: &ApprovalRequest) -> PortResult<ForeignRef>;
    async fn get_approval(&self, id: &str) -> PortResult<ApprovalView>;
}

// ── Windmill ────────────────────────────────────────────────────────────────

/// Versioned input to the generation procedure (request_version = 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationJob {
    pub request_version: i32,
    pub generation_request_id: String,
    pub windmill_path: String,
    pub engine_instance_id: String,
    pub engine_base_url: String,
    /// API-format ComfyUI graph with the bound inputs already applied, one per variant.
    pub prompts: Vec<Value>,
    /// Hub-chosen prompt ids, one per prompt, sent as `prompt_id` so a lost
    /// submission response can still be reconciled by id.
    pub prompt_ids: Vec<String>,
    /// `client_id` for `/prompt` so the hub's `/ws` listener receives progress.
    pub comfy_client_id: String,
    /// Hub endpoint the procedure posts receipts to.
    pub receipt_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicationTimer {
    pub intent_id: String,
    pub schedule_revision: i32,
    pub due_at: DateTime<Utc>,
    pub dispatch_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobView {
    pub id: String,
    pub status: JobStatus,
    pub raw: Value,
}

#[async_trait]
pub trait Windmill: Send + Sync {
    /// Starts the async, version-pinned generation procedure; returns the root job id.
    async fn run_generation(&self, job: &GenerationJob) -> PortResult<String>;
    /// Arms the publication timer; returns the job id. The hub rejects stale revisions.
    async fn schedule_publication(&self, timer: &PublicationTimer) -> PortResult<String>;
    async fn job(&self, id: &str) -> PortResult<JobView>;
    async fn cancel_job(&self, id: &str) -> PortResult<()>;
}

// ── ComfyUI ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputFile {
    pub node_id: String,
    pub filename: String,
    pub subfolder: String,
    pub kind: String,
}

/// Whole-prompt outcome from `/history/{prompt_id}` (not per-node events).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PromptOutcome {
    /// Not in history: queued, running, or unknown to the engine.
    Pending {
        queue_position: Option<usize>,
        running: bool,
    },
    Succeeded {
        outputs: Vec<OutputFile>,
    },
    Failed {
        message: String,
    },
    /// Neither queued, running nor in history (history may have been cleared).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CancelOutcome {
    Dequeued,
    Interrupted,
    AlreadyFinished,
    /// Targeted cancellation not supported and the prompt is running; we
    /// refuse to interrupt globally.
    Unsupported(String),
}

/// Progress hints from `/ws`. Display only: a node's `executed` output is not
/// whole-workflow success; terminal state comes from `outcome`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressEvent {
    Started {
        prompt_id: String,
    },
    Executing {
        prompt_id: String,
        node: String,
    },
    Progress {
        prompt_id: String,
        node: Option<String>,
        value: u64,
        max: u64,
    },
    NodeExecuted {
        prompt_id: String,
        node: String,
    },
    /// `execution_success`: a hint to reconcile now.
    Finished {
        prompt_id: String,
    },
    Error {
        prompt_id: String,
        message: String,
    },
    Interrupted {
        prompt_id: String,
    },
}

/// Prompt ids the engine reports in `/queue`, ours or not.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineLoad {
    pub running: Vec<String>,
    pub pending: Vec<String>,
}

#[async_trait]
pub trait Comfy: Send + Sync {
    /// Current queue, for GPU admission.
    async fn load(&self, base_url: &str) -> PortResult<EngineLoad>;
    /// Streams progress for prompts submitted with `client_id` until the socket
    /// closes; the caller reconnects and reconciles through `outcome`.
    async fn subscribe(&self, base_url: &str, client_id: &str, tx: tokio::sync::mpsc::Sender<ProgressEvent>) -> PortResult<()>;
    async fn outcome(&self, base_url: &str, prompt_id: &str) -> PortResult<PromptOutcome>;
    async fn fetch(&self, base_url: &str, file: &OutputFile) -> PortResult<Vec<u8>>;
    async fn cancel(&self, base_url: &str, prompt_id: &str) -> PortResult<CancelOutcome>;
}

// ── Pipelines (additive generation backend) ─────────────────────────────────

/// Versioned input to a registered pipeline (request_version = 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineRequest {
    pub request_version: i32,
    pub generation_request_id: Uuid,
    /// Registered pipeline id, e.g. `studio.render`.
    pub pipeline_id: String,
    /// Pipeline-specific inputs; the pipeline validates them.
    pub inputs: Value,
    /// Stable per attempt.
    pub idempotency_key: String,
    /// Hub endpoint the pipeline posts receipts to.
    pub receipt_url: String,
    /// Hub endpoint that accepts artifact bytes (`POST`).
    pub artifact_url: String,
}

/// Opaque external handle for a pipeline run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineRun {
    pub external_id: String,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PipelineStatus {
    Pending,
    Running {
        progress: Option<f32>,
    },
    /// Deferred by the pipeline (e.g. VRAM busy); the worker defers the
    /// outbox row instead of spending a retry.
    Deferred {
        reason: String,
    },
    /// The complete output manifest.
    Succeeded {
        artifacts: Vec<contracts::ArtifactRef>,
    },
    Failed {
        reason: String,
    },
    Cancelled,
}

/// A registered generation pipeline. ComfyUI stays the built-in path; this is
/// an additive second backend selected by `workflows.pipeline_id`.
#[async_trait]
pub trait Pipeline: Send + Sync {
    async fn submit(&self, req: &PipelineRequest) -> PortResult<PipelineRun>;
    async fn status(&self, run: &PipelineRun) -> PortResult<PipelineStatus>;
    async fn cancel(&self, run: &PipelineRun) -> PortResult<CancelOutcome>;
    /// The run an earlier submission of this request started, if the backend
    /// still knows it. Read when the hub holds a submission claim but never
    /// recorded the handle (a crash between the two), so the run is adopted
    /// instead of submitted twice. `Ok(None)` means the backend has no such
    /// run; the default implements a backend that cannot answer.
    async fn find(&self, _req: &PipelineRequest) -> PortResult<Option<PipelineRun>> {
        Ok(None)
    }
}

// ── Publishing & engagement transport ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaBlob {
    pub sha256: String,
    pub mime: String,
    pub alt_text: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishRequest {
    pub account_id: String,
    pub platform: String,
    /// Stable per delivery attempt; providers that support it dedupe on it.
    pub idempotency_key: String,
    pub text: String,
    pub media: Vec<MediaBlob>,
    pub platform_settings: Value,
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderState {
    /// Transport accepted it; not yet confirmed on the platform.
    Accepted,
    Published,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPost {
    pub provider_id: String,
    pub state: ProviderState,
    pub url: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyRequest {
    pub account_id: String,
    pub idempotency_key: String,
    pub thread_id: String,
    pub in_reply_to: Option<String>,
    pub text: String,
}

#[async_trait]
pub trait Publisher: Send + Sync {
    async fn submit(&self, req: &PublishRequest) -> PortResult<ProviderPost>;
    /// Reconciliation read by idempotency key; `None` means the provider has no record.
    async fn lookup(&self, account_id: &str, idempotency_key: &str) -> PortResult<Option<ProviderPost>>;
    async fn reply(&self, req: &ReplyRequest) -> PortResult<ProviderPost>;
    async fn lookup_reply(&self, account_id: &str, idempotency_key: &str) -> PortResult<Option<ProviderPost>>;
}

// ── Durable media ───────────────────────────────────────────────────────────

#[async_trait]
pub trait AssetStore: Send + Sync {
    /// Writes to a temporary object, fsyncs, verifies the checksum and moves it
    /// into place; returns the storage key only once the object is final.
    async fn put(&self, sha256: &str, bytes: &[u8]) -> std::io::Result<String>;
    async fn get(&self, key: &str) -> std::io::Result<Vec<u8>>;
}

// ── Creative assistant (local inference) ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantTurn {
    pub role: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantReply {
    pub body: String,
    /// A suggested canon change; recorded as a proposal, never applied.
    pub canon_suggestion: Option<contracts::CanonBody>,
    pub model: String,
}

#[async_trait]
pub trait Assistant: Send + Sync {
    /// `context` carries the brief, accepted canon and linked Paperclip work.
    async fn reply(&self, context: &Value, turns: &[AssistantTurn]) -> PortResult<AssistantReply>;
}

// ── Shared-GPU peers ────────────────────────────────────────────────────────

/// Another GPU tenant (llama-swap) whose loaded models occupy the device.
#[async_trait]
pub trait GpuPeer: Send + Sync {
    /// True when any model is loaded or loading at `running_url` (llama-swap `GET /running`).
    async fn busy(&self, running_url: &str) -> PortResult<bool>;
}
