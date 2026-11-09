//! Versioned JSON wire contracts (`/v1`).
//!
//! One source of wire truth shared by the API, worker, desktop UI and CLI.
//! Adapted from the donor's `aivyx-ipc` pattern (a dependency-light crate with
//! no async/storage deps, so every client serializes from the same types); the
//! Aivyx daemon/mission payloads were replaced by hub domain types.
//! See `docs/source-provenance.md`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[cfg(feature = "client")]
pub mod client;

/// Contract version carried in the URL prefix and in hashed payloads.
pub const API_VERSION: &str = "v1";
/// Request/response header carrying the shared correlation id.
pub const CORRELATION_HEADER: &str = "x-correlation-id";
/// Header carrying the sender's idempotency key on ingest endpoints.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

macro_rules! wire_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $wire:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $wire),+ }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($wire => Some($name::$variant),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

wire_enum!(
    /// Who is calling. Agents get scoped hub tools only; publisher and human
    /// approval credentials never map to `Agent`.
    Role { Operator = "operator", Agent = "agent", Procedure = "procedure", Ingest = "ingest" }
);

wire_enum!(CanonState { Proposed = "proposed", Accepted = "accepted", Rejected = "rejected", Superseded = "superseded" });
wire_enum!(CanonSource { Operator = "operator", Chat = "chat", News = "news", Agent = "agent" });
wire_enum!(ContentState { Idea = "idea", Brief = "brief", Producing = "producing", Selecting = "selecting", Review = "review", Scheduled = "scheduled", Published = "published", Archived = "archived" });
wire_enum!(GenerationState { Requested = "requested", Dispatched = "dispatched", Running = "running", Succeeded = "succeeded", Failed = "failed", Cancelling = "cancelling", Cancelled = "cancelled", OutcomeUnknown = "outcome_unknown" });
wire_enum!(AttemptKind { WindmillRoot = "windmill_root", WindmillChild = "windmill_child", ComfyPrompt = "comfy_prompt" });
wire_enum!(AttemptState { Submitted = "submitted", Queued = "queued", Running = "running", Succeeded = "succeeded", Failed = "failed", Cancelled = "cancelled", OutcomeUnknown = "outcome_unknown" });
wire_enum!(AssetState { Importing = "importing", Ready = "ready", Failed = "failed" });
wire_enum!(ReleaseState { Draft = "draft", InReview = "in_review", Approved = "approved", Rejected = "rejected", Superseded = "superseded" });
wire_enum!(IntentState { Scheduled = "scheduled", Claimed = "claimed", Submitting = "submitting", Accepted = "accepted", Published = "published", Failed = "failed", OutcomeUnknown = "outcome_unknown", Cancelled = "cancelled" });
wire_enum!(Owner { Agent = "agent", Human = "human" });
wire_enum!(Direction { Inbound = "inbound", Outbound = "outbound" });
wire_enum!(DraftState { Draft = "draft", InReview = "in_review", Approved = "approved", Queued = "queued", Sent = "sent", Invalidated = "invalidated", OutcomeUnknown = "outcome_unknown", HandedOff = "handed_off", Rejected = "rejected" });
wire_enum!(OutboxState { Pending = "pending", Delivered = "delivered", Dead = "dead" });
wire_enum!(
    /// How far an integration has actually been proven.
    Verification { Built = "built", FixtureTested = "fixture_tested", ContractVerified = "contract_verified", LiveVerified = "live_verified" }
);
wire_enum!(AdapterMode { Fixture = "fixture", Live = "live", Disabled = "disabled" });

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    /// Stable machine code, e.g. `stale_schedule_revision`.
    pub code: String,
    pub message: String,
    pub correlation_id: Option<String>,
}

// ── System ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationStatus {
    pub name: String,
    pub mode: AdapterMode,
    pub verification: Verification,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemStatus {
    pub api_version: String,
    /// True when any adapter runs on fixtures; the UI must show a banner.
    pub fixture_mode: bool,
    pub production_paused: bool,
    pub principal: Principal,
    pub integrations: Vec<IntegrationStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    pub id: Uuid,
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PauseIn {
    pub paused: bool,
    pub reason: Option<String>,
}

// ── Personas & canon ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persona {
    pub id: Uuid,
    pub company_id: String,
    pub name: String,
    /// The newest accepted canon, if any.
    pub canon: Option<CanonRevision>,
    pub pending_proposals: i64,
}

/// Persona voice/brand rules plus accepted world canon.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CanonBody {
    pub voice: String,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub avoid: Vec<String>,
    #[serde(default)]
    pub world: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonRevision {
    pub id: Uuid,
    pub persona_id: Uuid,
    pub revision: i32,
    /// Revision this proposal was written against (stale bases are rejected).
    pub base_revision: Option<i32>,
    pub body: CanonBody,
    pub state: CanonState,
    pub source: CanonSource,
    pub rationale: Option<String>,
    pub proposed_by: String,
    pub decided_by: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonProposalIn {
    pub base_revision: Option<i32>,
    pub body: CanonBody,
    pub source: CanonSource,
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum CanonDecisionIn {
    Accept,
    /// Accept with the operator's edits applied (donor "edit & approve").
    AcceptEdited { body: CanonBody },
    Reject { reason: Option<String> },
}

// ── News ────────────────────────────────────────────────────────────────────

/// Normalized news event as posted by n8n or Windmill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewsEventIn {
    /// `n8n:<workflow>` or `windmill:<path>`.
    pub source: String,
    /// The sender's stable event id; retries must reuse it.
    pub external_id: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    pub published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub url: String,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestReceipt {
    pub id: Uuid,
    /// False when the event was a duplicate delivery (already persisted).
    pub created: bool,
    pub revision: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewsItem {
    pub id: Uuid,
    pub canonical_url: String,
    pub title: String,
    pub summary: String,
    pub sources: Vec<String>,
    /// Number of deliveries (including retries and other feeds) seen.
    pub coverage: i64,
    pub revision: i32,
    pub evidence: Vec<Evidence>,
    pub first_seen_at: DateTime<Utc>,
    pub content_item_id: Option<Uuid>,
}

// ── Content & briefs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub angle: String,
    pub audience: String,
    pub copy_direction: String,
    pub visual_direction: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateBriefIn {
    pub persona_id: Uuid,
    pub news_item_id: Option<Uuid>,
    pub title: String,
    pub brief: Brief,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentItem {
    pub id: Uuid,
    pub persona_id: Uuid,
    pub news_item_id: Option<Uuid>,
    pub title: String,
    pub brief: Brief,
    pub state: ContentState,
    pub paperclip_issue_id: Option<String>,
    pub paperclip_issue_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentDetail {
    pub item: ContentItem,
    pub generations: Vec<GenerationRequest>,
    pub assets: Vec<Asset>,
    pub releases: Vec<ReleaseRevision>,
    pub intents: Vec<PublicationIntent>,
    pub history: Vec<HistoryEntry>,
}

// ── Production ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub id: Uuid,
    pub name: String,
    pub version: i32,
    pub engine_instance_id: String,
    pub graph_sha256: String,
    /// Named inputs: `{ "prompt": {"node": "6", "input": "text"} }`.
    pub bindings: Value,
    pub windmill_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateIn {
    pub workflow_id: Uuid,
    /// Values for the workflow's named bindings.
    pub inputs: Value,
    pub variants: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub id: Uuid,
    pub content_item_id: Uuid,
    pub workflow_id: Uuid,
    /// Contract version of the generation request payload.
    pub request_version: i32,
    pub inputs: Value,
    pub variants: i32,
    pub state: GenerationState,
    pub attempts: Vec<ExecutionAttempt>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionAttempt {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub kind: AttemptKind,
    pub external_id: String,
    pub engine_instance_id: Option<String>,
    pub state: AttemptState,
    pub progress: Value,
    pub updated_at: DateTime<Utc>,
}

/// Posted by the Windmill procedure after it submits to ComfyUI, so the hub
/// can reconcile, cancel and recover the actual engine work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReceiptIn {
    pub kind: AttemptKind,
    pub external_id: String,
    /// External id of the parent attempt (e.g. the Windmill root job).
    pub parent_external_id: Option<String>,
    pub engine_instance_id: Option<String>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: Uuid,
    pub content_item_id: Uuid,
    pub generation_request_id: Option<Uuid>,
    pub sha256: String,
    pub mime: String,
    pub bytes: i64,
    pub state: AssetState,
    pub selected: bool,
    pub alt_text: String,
    /// prompt id, engine instance, workflow hash, node id, filename.
    pub provenance: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectAssetIn {
    pub selected: bool,
    pub alt_text: Option<String>,
}

// ── Releases & publication ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub company_id: String,
    pub platform: String,
    pub handle: String,
    pub transport: String,
    pub persona_id: Option<Uuid>,
    pub capabilities: AccountCapabilities,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountCapabilities {
    pub publish: bool,
    pub comments: bool,
    pub mentions: bool,
    pub replies: bool,
    pub dms: bool,
    pub analytics: bool,
}

/// Inclusive window within which an approved release may be (re)scheduled
/// without a new review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingScope {
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaRef {
    pub asset_id: Uuid,
    pub sha256: String,
    pub mime: String,
    pub alt_text: String,
}

/// The complete frozen action an approval authorizes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseAction {
    pub company_id: String,
    pub account_id: String,
    pub text: String,
    pub media: Vec<MediaRef>,
    #[serde(default)]
    pub platform_settings: Value,
    pub reply_to: Option<String>,
    pub timing: TimingScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FreezeReleaseIn {
    pub account_ids: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub platform_settings: Value,
    pub reply_to: Option<String>,
    pub timing: TimingScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseRevision {
    pub id: Uuid,
    pub content_item_id: Uuid,
    pub revision: i32,
    pub actions: Vec<ReleaseAction>,
    /// `hub-release-v1:<sha256 hex>` over the canonical JSON of `actions`.
    pub payload_hash: String,
    pub state: ReleaseState,
    pub paperclip_approval_id: Option<String>,
    pub approved_by: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleIn {
    pub due_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicationIntent {
    pub id: Uuid,
    pub release_revision_id: Uuid,
    pub account_id: String,
    pub schedule_revision: i32,
    pub due_at: DateTime<Utc>,
    pub state: IntentState,
    pub provider_post_id: Option<String>,
    pub provider_url: Option<String>,
    pub attempts: Vec<DeliveryAttempt>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveryAttempt {
    pub id: Uuid,
    pub attempt: i32,
    pub idempotency_key: String,
    pub state: IntentState,
    pub detail: Value,
    pub created_at: DateTime<Utc>,
}

/// Sent by the Windmill publication timer at the due time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchIn {
    pub schedule_revision: i32,
    /// Windmill job id of the timer firing, for the receipt trail.
    pub trigger_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchOut {
    pub intent: PublicationIntent,
    /// False when this call converged on an earlier claim (duplicate timer).
    pub submitted: bool,
}

// ── Relationships ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionIn {
    pub account_id: String,
    pub external_thread_id: String,
    pub external_id: String,
    pub direction: Direction,
    pub author: String,
    pub body: String,
    pub occurred_at: DateTime<Utc>,
    pub permalink: Option<String>,
    #[serde(default)]
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    pub id: Uuid,
    pub account_id: String,
    pub external_thread_id: String,
    pub counterpart: String,
    pub owner: Owner,
    pub owner_user: Option<String>,
    pub owner_epoch: i32,
    pub follow_up_at: Option<DateTime<Utc>>,
    pub last_inbound_at: Option<DateTime<Utc>>,
    pub content_item_id: Option<Uuid>,
    pub permalink: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    pub id: Uuid,
    pub direction: Direction,
    pub external_id: String,
    pub author: String,
    pub body: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplyDraft {
    pub id: Uuid,
    pub relationship_id: Uuid,
    pub in_reply_to: Option<Uuid>,
    pub body: String,
    pub author_kind: Owner,
    pub author: String,
    pub owner_epoch: i32,
    pub state: DraftState,
    pub reviewed_by: Option<String>,
    pub provider_reply_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationshipDetail {
    pub relationship: Relationship,
    pub interactions: Vec<Interaction>,
    pub drafts: Vec<ReplyDraft>,
    pub history: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftReplyIn {
    pub in_reply_to: Option<Uuid>,
    pub body: String,
    /// The owner epoch the author saw; stale epochs are rejected.
    pub owner_epoch: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum DraftDecisionIn {
    Approve,
    Reject { reason: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowUpIn {
    pub follow_up_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffOut {
    pub url: String,
}

// ── Today, history, chat, search ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub subject_type: String,
    pub subject_id: String,
    pub correlation_id: Option<String>,
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodayItem {
    pub kind: String,
    pub title: String,
    pub subject_type: String,
    pub subject_id: Uuid,
    pub status: String,
    pub at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Today {
    pub pending_reviews: Vec<TodayItem>,
    pub active_production: Vec<TodayItem>,
    pub due_releases: Vec<TodayItem>,
    pub failed_operations: Vec<TodayItem>,
    pub overdue_interactions: Vec<TodayItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessageIn {
    pub body: String,
}

/// Creative chat bound to a content item or persona. The assistant may attach
/// a canon proposal; it is never applied without review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: Uuid,
    pub subject_type: String,
    pub subject_id: Uuid,
    pub role: String,
    pub author: String,
    pub body: String,
    pub proposal_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub subject_type: String,
    pub subject_id: Uuid,
    pub title: String,
    pub snippet: String,
    pub rank: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelOut {
    pub state: GenerationState,
    /// What was done per engine attempt (dequeued, interrupted, unsupported…).
    pub actions: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_match_serde() {
        for s in IntentState::ALL {
            let json = serde_json::to_string(s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(IntentState::parse(s.as_str()), Some(*s));
        }
        assert_eq!(AttemptKind::ComfyPrompt.as_str(), "comfy_prompt");
        assert_eq!(GenerationState::OutcomeUnknown.as_str(), "outcome_unknown");
    }
}
