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

/// The fleet operations snapshot (`fleetix::status`), re-exported so the API,
/// desktop and CLI never disagree about its shape.
pub use fleetix::status as fleet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretInventory {
    pub schema_version: u32,
    pub generated_at: String,
    pub secrets: Vec<Secret>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretInventoryView {
    pub schema_version: u32,
    pub generated_at: String,
    pub secrets: Vec<SecretView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretView {
    #[serde(flatten)]
    pub secret: Secret,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    pub id: String,
    pub tree: String,
    pub scope: String,
    pub category: String,
    pub project: Option<String>,
    pub kind: String,
    pub custodian: String,
    pub expiry: SecretExpiry,
    pub rotated_at: Option<String>,
    /// `null` until the secret's rotation how-to is documented.
    #[serde(default)]
    pub rotation: Option<SecretRotation>,
    pub consumers: Vec<SecretConsumer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SecretExpiry {
    At { date: chrono::NaiveDate },
    Never,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRotation {
    pub method: String,
    pub command: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SecretConsumer {
    File { path: String },
    Env { name: String },
}

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
    /// approval credentials never map to `Agent`. A member is a second human:
    /// it sees and dismisses only its own notifications.
    Role { Operator = "operator", Agent = "agent", Procedure = "procedure", Ingest = "ingest", Member = "member" }
);

wire_enum!(CanonState { Proposed = "proposed", Accepted = "accepted", Rejected = "rejected", Superseded = "superseded" });
wire_enum!(ProjectKind { Solo = "solo", Group = "group" });
wire_enum!(CanonSource { Operator = "operator", Chat = "chat", News = "news", Agent = "agent" });
wire_enum!(ContentState { Idea = "idea", Brief = "brief", Producing = "producing", Selecting = "selecting", Review = "review", Approved = "approved", Scheduled = "scheduled", Published = "published", Archived = "archived" });
wire_enum!(GenerationState { Requested = "requested", Dispatched = "dispatched", Running = "running", Succeeded = "succeeded", Failed = "failed", Cancelling = "cancelling", Cancelled = "cancelled", OutcomeUnknown = "outcome_unknown" });
wire_enum!(AttemptKind {
    WindmillRoot = "windmill_root",
    WindmillChild = "windmill_child",
    ComfyPrompt = "comfy_prompt",
    PipelineRun = "pipeline_run"
});
wire_enum!(AttemptState { Submitted = "submitted", Queued = "queued", Running = "running", Succeeded = "succeeded", Failed = "failed", Cancelled = "cancelled", OutcomeUnknown = "outcome_unknown" });
wire_enum!(AssetState { Importing = "importing", Ready = "ready", Failed = "failed" });
wire_enum!(ReleaseState { Draft = "draft", InReview = "in_review", Approved = "approved", Rejected = "rejected", Superseded = "superseded" });
wire_enum!(IntentState { Scheduled = "scheduled", Claimed = "claimed", Submitting = "submitting", Accepted = "accepted", Published = "published", Failed = "failed", OutcomeUnknown = "outcome_unknown", Cancelled = "cancelled" });
wire_enum!(Owner { Agent = "agent", Human = "human" });
wire_enum!(Direction { Inbound = "inbound", Outbound = "outbound" });
wire_enum!(DraftState { Draft = "draft", InReview = "in_review", Approved = "approved", Queued = "queued", Sending = "sending", Sent = "sent", Invalidated = "invalidated", OutcomeUnknown = "outcome_unknown", HandedOff = "handed_off", Rejected = "rejected" });
wire_enum!(OutboxState { Pending = "pending", Delivered = "delivered", Dead = "dead" });
wire_enum!(
    /// How far an integration has actually been proven.
    Verification { Built = "built", FixtureTested = "fixture_tested", ContractVerified = "contract_verified", LiveVerified = "live_verified" }
);
wire_enum!(AdapterMode { Fixture = "fixture", Live = "live", Disabled = "disabled" });

wire_enum!(
    #[doc = "Terminal state of a generation job. The wire names match the Windmill port enum; this copy lets receipts carry a terminal status in the contract crate."]
    JobStatus { Queued = "queued", Running = "running", Succeeded = "succeeded", Failed = "failed", Cancelled = "cancelled" }
);

wire_enum!(
    #[doc = "Who produces a brief's work: the delegated Paperclip studio org or the hub running a pipeline directly. Publication stays hub-owned in both modes."]
    Producer { Delegated = "delegated", Direct = "direct" }
);

// `wire_enum!` has no `#[default]` passthrough, so the impl stays explicit.
#[allow(clippy::derivable_impls)]
impl Default for Producer {
    fn default() -> Self {
        Self::Delegated
    }
}

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
    /// Status feeds this process polls (`HUB_FEEDS_CONFIG`); empty when none
    /// are configured or the producer loop is not running here.
    #[serde(default)]
    pub feeds: Vec<FeedState>,
}

/// One status feed as the last poll saw it. Failing checks themselves are
/// notifications keyed `monitor:<feed>:<check id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedState {
    pub id: String,
    /// `s3`.
    pub kind: String,
    /// `pending` (not polled yet), `ok`, `stale` or `error`.
    pub state: String,
    pub checked_at: Option<DateTime<Utc>>,
    /// `metadata.timestamp` of the last document read.
    pub generated_at: Option<DateTime<Utc>>,
    /// Checks at or above the feed's `min_status` in that document.
    pub raised: u32,
    /// Why the feed is `stale` or `error`.
    pub problem: Option<String>,
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

pub const MODEL_ROUTES: &[(&str, &str, &[&str])] = &[
    ("checker", "openai/gpt-6.1-sol", &["low", "medium", "high", "xhigh"]),
    ("image", "moss-look", &[]),
    ("assistant", "hub-default", &[]),
    ("three_d", "trellis2", &[]),
    ("voice", "kokoro", &[]),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRoute {
    pub id: String,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectModels {
    pub checker: ModelRoute,
    pub image: String,
    pub assistant: String,
    pub three_d: String,
    pub voice: String,
}

impl Default for ProjectModels {
    fn default() -> Self {
        Self {
            checker: ModelRoute { id: "openai/gpt-6.1-sol".into(), effort: Some("xhigh".into()) },
            image: "moss-look".into(),
            assistant: "hub-default".into(),
            three_d: "trellis2".into(),
            voice: "kokoro".into(),
        }
    }
}

impl ProjectModels {
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        for (role, id, effort) in [
            ("checker", self.checker.id.as_str(), self.checker.effort.as_deref()),
            ("image", self.image.as_str(), None),
            ("assistant", self.assistant.as_str(), None),
            ("three_d", self.three_d.as_str(), None),
            ("voice", self.voice.as_str(), None),
        ] {
            let Some((_, _, efforts)) = MODEL_ROUTES.iter().find(|(r, model, _)| *r == role && *model == id) else {
                return Err(("unknown_model", format!("unknown {role} model: {id}")));
            };
            if effort.is_some_and(|value| !efforts.contains(&value)) {
                return Err(("unknown_effort", format!("unknown effort for {role}: {}", effort.unwrap_or_default())));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub company_id: String,
    pub name: String,
    pub kind: ProjectKind,
    pub premise: String,
    pub dynamics: String,
    #[serde(default)]
    pub models: ProjectModels,
    pub members: Vec<ProjectMember>,
    pub account_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectMember {
    pub persona_id: Uuid,
    pub name: String,
    pub role: String,
    pub has_accepted_canon: bool,
    #[serde(default)]
    pub reference_asset_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectIn {
    pub name: String,
    pub kind: ProjectKind,
    #[serde(default)]
    pub premise: String,
    #[serde(default)]
    pub dynamics: String,
    #[serde(default)]
    pub models: Option<ProjectModels>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectMemberIn {
    #[serde(default)]
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectAccountsIn {
    pub account_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Persona {
    pub id: Uuid,
    pub company_id: String,
    pub name: String,
    #[serde(default)]
    pub profile: IndividualProfile,
    /// The newest accepted canon, if any.
    pub canon: Option<CanonRevision>,
    pub pending_proposals: i64,
    #[serde(default)]
    pub reference_asset_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadsAs {
    Adult,
    NonHuman,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IndividualProfile {
    #[serde(default)]
    pub bio: String,
    #[serde(default)]
    pub reads_as: Option<ReadsAs>,
    #[serde(default)]
    pub look: String,
    #[serde(default)]
    pub palette: Vec<String>,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub canon_draft: Option<CanonBody>,
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
    #[serde(default)]
    pub review_requested_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub review_issue_url: Option<String>,
    #[serde(default)]
    pub comments: Vec<CanonComment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonComment {
    pub id: Uuid,
    pub revision_id: Uuid,
    pub author: String,
    pub author_role: Role,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonCommentIn {
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonDraftIn {
    pub bio: String,
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
    AcceptEdited {
        body: CanonBody,
    },
    Reject {
        reason: Option<String>,
    },
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

// ── Opportunities ───────────────────────────────────────────────────────────

wire_enum!(OpportunityKind {
    Grant = "grant",
    Fellowship = "fellowship",
    Prize = "prize",
    Venue = "venue",
    Program = "program",
    Other = "other"
});

wire_enum!(OpportunityState {
    New = "new",
    Shortlisted = "shortlisted",
    Drafting = "drafting",
    Submitted = "submitted",
    Awarded = "awarded",
    Rejected = "rejected",
    Declined = "declined"
});

/// One funding call, fellowship, prize, venue or programme filed by a feed or
/// a scout. The identity is `(company_id, source, external_id)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpportunityIn {
    pub persona_id: Option<Uuid>,
    pub source: String,
    /// The sender's stable id; a replay updates facts instead of duplicating.
    pub external_id: String,
    pub kind: OpportunityKind,
    pub title: String,
    pub funder: String,
    pub url: String,
    /// Null means rolling or unknown.
    pub deadline: Option<DateTime<Utc>>,
    pub amount: Option<String>,
    #[serde(default)]
    pub eligibility: String,
    #[serde(default)]
    pub fit: String,
    /// At least one entry; findings without a source are refused.
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Opportunity {
    pub id: Uuid,
    pub company_id: String,
    pub persona_id: Option<Uuid>,
    pub source: String,
    pub external_id: String,
    pub kind: OpportunityKind,
    pub title: String,
    pub funder: String,
    pub url: String,
    pub deadline: Option<DateTime<Utc>>,
    pub amount: Option<String>,
    pub eligibility: String,
    pub fit: String,
    pub evidence: Vec<Evidence>,
    pub state: OpportunityState,
    pub note: Option<String>,
    pub paperclip_issue_id: Option<String>,
    /// A deadline in the past while the state is still open. Derived on read;
    /// the stored state is untouched.
    pub expired: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpportunityDecisionIn {
    pub state: OpportunityState,
    pub note: Option<String>,
    pub paperclip_issue_id: Option<String>,
}

wire_enum!(
    /// Where a renewal's expiry comes from (D14). `never` is a kind of its own,
    /// never a missing date or a far-future sentinel.
    ExpiryKind { Cadence = "cadence", Never = "never", Provider = "provider" }
);

// `wire_enum!` owns the derive list, so the default cannot be `#[default]`.
#[allow(clippy::derivable_impls)]
impl Default for ExpiryKind {
    fn default() -> Self {
        Self::Cadence
    }
}

wire_enum!(
    /// A provider's answer about a credential, reduced to fixed codes before
    /// it reaches the hub. Provider error text never travels here.
    Observation { Date = "date", Never = "never", Unverifiable = "unverifiable", Inactive = "inactive", NotYetValid = "not_yet_valid" }
);

/// One credential or account deadline: `(company_id, key)` is the seed's
/// stable identity, and only the operator renews it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Renewal {
    pub id: Uuid,
    pub company_id: String,
    pub key: String,
    /// The channel account this credential unlocks, when it maps to one.
    pub account_id: Option<String>,
    pub title: String,
    /// How to renew; shown to the operator.
    pub procedure: String,
    /// Provider pages the operator opens to mint the new value; https only.
    #[serde(default)]
    pub urls: Vec<String>,
    /// Null means the next due date is not known; a renewal asks for one.
    pub cadence_days: Option<i32>,
    pub warn_days: i32,
    /// Null means unknown; the row shows on Today until the first renewal.
    pub due_at: Option<DateTime<Utc>>,
    pub renewed_at: Option<DateTime<Utc>>,
    /// How this renewal's expiry is governed; seeds refresh it, the clock does
    /// not.
    #[serde(default)]
    pub expiry_kind: ExpiryKind,
    /// The last provider answer, null until the first observation.
    #[serde(default)]
    pub observation: Option<Observation>,
    /// When that answer was observed.
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
    /// When the last successful (`date` or `never`) answer was observed.
    #[serde(default)]
    pub last_good_at: Option<DateTime<Utc>>,
    /// The deadline the last successful answer reported; null for `never`.
    #[serde(default)]
    pub last_good_expires_at: Option<DateTime<Utc>>,
    /// Derived on read by the hub's judgement, never stored: `ok`, `due`,
    /// `overdue`, `unknown`, `never`, `unverifiable` or `invalid`.
    #[serde(default)]
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RenewalRenewedIn {
    /// RFC 3339; absent means `now + cadence_days`.
    pub due_at: Option<DateTime<Utc>>,
}

/// `POST /v1/renewals/{key}/observe`: one provider answer about a credential.
/// An observation is not a rotation and never moves `due_at` or `renewed_at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserveRenewalIn {
    pub outcome: Observation,
    /// Required with `date`, refused with every other outcome.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// RFC 3339; refused when more than 60 s in the future.
    pub observed_at: DateTime<Utc>,
}

// ── Notifications ───────────────────────────────────────────────────────────

wire_enum!(Severity { Info = "info", Warning = "warning", Critical = "critical" });

/// One condition the operator should see. `seq` is the long-poll cursor and
/// moves on every change; `open` is derived server-side from the three
/// timestamps (unresolved, unacknowledged, not snoozed into the future).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: Uuid,
    pub seq: i64,
    pub source: String,
    pub dedupe_key: String,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub subject_type: Option<String>,
    pub subject_id: Option<String>,
    pub url: Option<String>,
    pub read_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub snoozed_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub open: bool,
}

/// `POST /v1/notifications`; `source` defaults to `agent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaiseNotificationIn {
    #[serde(default)]
    pub source: Option<String>,
    pub dedupe_key: String,
    pub severity: Severity,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub subject_type: Option<String>,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Rows newer than the caller's cursor, newest first, plus the cursor to send
/// back. `open` and `critical` always count *all* open rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NotificationPage {
    pub items: Vec<Notification>,
    pub cursor: i64,
    pub open: i64,
    pub critical: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AckNotificationsIn {
    #[serde(default)]
    pub ids: Vec<Uuid>,
    /// Acknowledge every open row instead of `ids`.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnoozeNotificationIn {
    /// RFC 3339; must be in the future.
    pub until: DateTime<Utc>,
}

/// `POST /v1/notifications/sync`: one process's whole current set for `source`,
/// the HTTP twin of a producer pass. Items carry no source — the pass owns it —
/// and the server namespaces a member's rows with its own name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncNotificationsIn {
    pub source: String,
    pub items: Vec<SyncNotificationItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncNotificationItem {
    pub dedupe_key: String,
    pub severity: Severity,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub subject_type: Option<String>,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// How many rows the pass announced or resolved; zero means nothing changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncNotificationsOut {
    pub changed: usize,
}

// ── Content & briefs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    pub angle: String,
    pub audience: String,
    pub copy_direction: String,
    pub visual_direction: String,
    /// Delegated creates the Paperclip issue (the historical behaviour);
    /// direct means the hub runs the pipeline itself.
    #[serde(default)]
    pub producer: Producer,
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
    /// The ComfyUI engine, or the admission target for a pipeline workflow.
    pub engine_instance_id: Option<String>,
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
    pub content_item_id: Option<Uuid>,
    pub persona_id: Option<Uuid>,
    pub source_asset_id: Option<Uuid>,
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
    /// Terminal status of a pipeline run; absent for ComfyUI receipts.
    #[serde(default)]
    pub status: Option<JobStatus>,
    /// Complete output manifest; required when `status` is `succeeded`.
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub raw: Value,
}

/// One durable output of a generation. `source` is the pipeline's stable name
/// for it and is unique within the generation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub source: String,
    pub sha256: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageCheck {
    pub check_key: String,
    pub verdict: String,
    pub checker: String,
    pub detail: String,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: Uuid,
    pub content_item_id: Option<Uuid>,
    pub persona_id: Option<Uuid>,
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
    #[serde(default)]
    pub checks: Vec<ImageCheck>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookSize {
    Test,
    Final,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookPassIn {
    pub source_asset_id: Option<Uuid>,
    pub prompt: String,
    pub strength: Option<f32>,
    pub variants: i32,
    pub size: LookSize,
    pub reuse_seeds_from: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LookPass {
    pub generation: GenerationRequest,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndividualLook {
    pub uploads: Vec<Asset>,
    pub passes: Vec<LookPass>,
    #[serde(default)]
    pub reference: Option<Reference>,
    #[serde(default)]
    pub reference_history: Vec<Reference>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    pub id: Uuid,
    pub persona_id: Uuid,
    pub asset_id: Uuid,
    pub sha256: String,
    pub pinned_by: String,
    pub pinned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinIn {
    pub asset_id: Uuid,
    #[serde(default)]
    pub confirm_repin: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinOut {
    pub reference: Reference,
    #[serde(default)]
    pub affected_projects: Vec<String>,
    #[serde(default)]
    pub stale_stages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookSourceIn {
    pub asset_id: Uuid,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SelectAssetIn {
    pub selected: bool,
    pub alt_text: Option<String>,
    #[serde(default)]
    pub attest: Vec<String>,
    #[serde(default)]
    pub advice: std::collections::BTreeMap<String, bool>,
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
    /// Transport channel (`accounts.transport`) recorded when the release is
    /// frozen; dispatch only uses the frozen value, never a live lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
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

/// Operator resolution of an `outcome_unknown` delivery after checking the
/// platform by hand (for transports without a reconciliation read).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolveIntentIn {
    pub published: bool,
    pub provider_post_id: Option<String>,
    pub provider_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkApprovalIn {
    pub approval_id: String,
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
    #[serde(default)]
    pub detail: Option<String>,
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
    /// Shortlisted or drafting calls due within 14 days (additive field).
    #[serde(default)]
    pub opportunities: Vec<TodayItem>,
    /// Renewals that are unknown or inside their warn window (additive field).
    #[serde(default)]
    pub renewals: Vec<TodayItem>,
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
    use serde_json::json;

    #[test]
    fn every_default_model_is_in_the_catalog() {
        assert!(ProjectModels::default().validate().is_ok());
    }

    #[test]
    fn individual_profile_defaults_and_preserves_bio() {
        let empty: IndividualProfile = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, IndividualProfile::default());
        let profile: IndividualProfile = serde_json::from_value(json!({"bio": "é  \n\n", "reads_as": "non_human"})).unwrap();
        assert_eq!(profile.bio, "é  \n\n");
        assert_eq!(profile.reads_as, Some(ReadsAs::NonHuman));
        assert_eq!(serde_json::from_value::<IndividualProfile>(serde_json::to_value(&profile).unwrap()).unwrap(), profile);
    }

    #[test]
    fn wire_names_match_serde() {
        for s in IntentState::ALL {
            let json = serde_json::to_string(s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(IntentState::parse(s.as_str()), Some(*s));
        }
        assert_eq!(AttemptKind::ComfyPrompt.as_str(), "comfy_prompt");
        assert_eq!(GenerationState::OutcomeUnknown.as_str(), "outcome_unknown");
        assert_eq!(JobStatus::Succeeded.as_str(), "succeeded");
    }

    /// The pack-contract fields are additive: JSON written before them still
    /// decodes, and the new values survive a round trip.
    #[test]
    fn pack_contract_fields_are_backward_compatible() {
        let old_receipt = json!({"kind": "comfy_prompt", "external_id": "p1", "parent_external_id": null, "engine_instance_id": null});
        let receipt: ReceiptIn = serde_json::from_value(old_receipt).unwrap();
        assert_eq!(receipt.status, None);
        assert!(receipt.artifacts.is_empty());

        let receipt = ReceiptIn {
            kind: AttemptKind::ComfyPrompt,
            external_id: "p1".into(),
            parent_external_id: None,
            engine_instance_id: None,
            status: Some(JobStatus::Succeeded),
            artifacts: vec![ArtifactRef { source: "render".into(), sha256: "ab".into(), mime: "video/mp4".into(), size: 12 }],
            raw: Value::Null,
        };
        let round: ReceiptIn = serde_json::from_value(serde_json::to_value(&receipt).unwrap()).unwrap();
        assert_eq!(round, receipt);

        let old_brief = json!({"angle": "a", "audience": "b", "copy_direction": "c", "visual_direction": "d"});
        let brief: Brief = serde_json::from_value(old_brief).unwrap();
        assert_eq!(brief.producer, Producer::Delegated);
        assert_eq!(serde_json::to_value(&brief).unwrap()["producer"], "delegated");

        assert_eq!(serde_json::from_value::<Producer>(json!("direct")).unwrap(), Producer::Direct);
    }

    /// A real-shaped snapshot: two hosts (one outdated), an alert, a pending
    /// approval and a failed nightly. Guards the contract the hub reads.
    const SNAPSHOT: &str = r#"{
      "schemaVersion": 1,
      "generatedAt": "2026-11-10T03:10:00Z",
      "targetRevision": "new",
      "hosts": [
        {"name": "atlas", "health": "healthy", "revision": "new", "system": "/nix/store/atlas", "failures": [], "checkedAt": "2026-11-10T03:09:00Z"},
        {"name": "nomad", "health": "degraded", "revision": "old", "failures": ["forgejo: inactive"], "checkedAt": "2026-11-10T03:09:30Z"}
      ],
      "monitoring": {"monitored": 12, "alerts": [
        {"key": "core_forgejo", "name": "forgejo", "group": "core", "since": "2026-11-10T02:00:00Z", "detail": ["[STATUS] == 200"]}
      ]},
      "pendingOps": [
        {"id": "op-1", "host": "nomad", "title": "restart forgejo", "reason": "unit is down", "requestedBy": "nightly", "created": "2026-11-10T03:00:00Z"}
      ],
      "nightly": {
        "startedAt": "2026-11-10T02:00:00Z",
        "finishedAt": "2026-11-10T02:20:00Z",
        "revision": "new",
        "commit": "0123abc",
        "error": null,
        "hosts": [{"host": "nomad", "outcome": "failed", "detail": "health check failed"}]
      }
    }"#;

    #[test]
    fn fleet_snapshot_round_trips_through_a_file() {
        let path = std::env::temp_dir().join(format!("fleet-snapshot-{}.json", Uuid::new_v4()));
        std::fs::write(&path, SNAPSHOT).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let status: fleet::FleetStatus = serde_json::from_str(&body).unwrap();
        assert_eq!(status.schema_version, fleet::SCHEMA_VERSION);
        assert_eq!(status.target_revision.as_deref(), Some("new"));
        assert_eq!(status.hosts[1].update_state(status.target_revision.as_deref()), fleet::UpdateState::Outdated);
        assert_eq!(status.monitoring.monitored, 12);
        assert_eq!(status.monitoring.alerts[0].group, "core");
        assert_eq!(status.pending_ops[0].id, "op-1");
        assert!(status.nightly.as_ref().unwrap().failed());
        assert_eq!(status.attention(), 4);
        assert_eq!(serde_json::from_str::<fleet::FleetStatus>(&serde_json::to_string(&status).unwrap()).unwrap(), status);
    }
}
