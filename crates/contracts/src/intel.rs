//! Intelligence ingestion, review and release contracts.

use super::{DateTime, Deserialize, Evidence, IngestReceipt, Serialize, Severity, Utc, Uuid, Value};

wire_enum!(Audience {
    OperatorOnly = "operator_only", CompanyInternal = "company_internal",
    PersonaEligible = "persona_eligible", PublicCleared = "public_cleared",
});

impl Audience {
    pub fn rank(&self) -> u8 {
        match self {
            Self::OperatorOnly => 3,
            Self::CompanyInternal => 2,
            Self::PersonaEligible => 1,
            Self::PublicCleared => 0,
        }
    }
}

wire_enum!(SignalKind {
    Change = "change", Metric = "metric", Mention = "mention", Announcement = "announcement", Release = "release",
    Funding = "funding", Partnership = "partnership", Risk = "risk", Trend = "trend", Ops = "ops",
});
wire_enum!(IntelRunStatus { Ok = "ok", Partial = "partial", Failed = "failed" });
wire_enum!(ProvenanceMethod { Observed = "observed", Derived = "derived", Llm = "llm" });
wire_enum!(SignalState { New = "new", Acknowledged = "acknowledged", Promoted = "promoted", Dismissed = "dismissed" });
wire_enum!(SubjectScheme {
    Domain = "domain", EmailDomain = "email_domain", SolanaAddress = "solana_address", Handle = "handle",
    Url = "url", Github = "github", Name = "name",
});
wire_enum!(LinkTargetType {
    News = "news", Opportunity = "opportunity", Relationship = "relationship", Content = "content",
    Project = "project", Persona = "persona", MailMessage = "mail_message", Signal = "signal",
});
wire_enum!(LinkRelation { About = "about", DerivedFrom = "derived_from", PromotedTo = "promoted_to", Supersedes = "supersedes" });
wire_enum!(SignalDecisionAction { Acknowledge = "acknowledge", Dismiss = "dismiss", Snooze = "snooze", Reopen = "reopen" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectRef {
    pub scheme: SubjectScheme,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalLinkIn {
    pub target_type: LinkTargetType,
    pub target_id: String,
    pub relation: LinkRelation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalLink {
    pub target_type: LinkTargetType,
    pub target_id: String,
    pub relation: LinkRelation,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalProvenance {
    pub producer: String,
    pub run_id: String,
    #[serde(default)]
    pub git_rev: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub method: ProvenanceMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_by: Option<String>,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRunIn {
    pub run_id: String,
    pub status: IntelRunStatus,
    pub finished_at: DateTime<Utc>,
    #[serde(default = "empty_object")]
    pub watermark: Value,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalIn {
    pub external_id: String,
    pub kind: SignalKind,
    pub severity: Severity,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    pub occurred_at: DateTime<Utc>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// When omitted or null, inherit the source's default audience.
    pub audience: Option<Audience>,
    pub confidence: f32,
    pub evidence: Vec<Evidence>,
    #[serde(default = "empty_object")]
    pub facts: Value,
    #[serde(default)]
    pub subjects: Vec<SubjectRef>,
    #[serde(default)]
    pub links: Vec<SignalLinkIn>,
    #[serde(default)]
    pub persona_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    pub provenance: SignalProvenance,
    #[serde(default)]
    pub backfill: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalBatchIn {
    pub source: String,
    pub run: SourceRunIn,
    #[serde(default)]
    pub signals: Vec<SignalIn>,
}

/// Only explicitly supplied redacted fields enter the signal. The message ID
/// is used for the access check, not the signal's provenance or links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromoteFromMailIn {
    pub mail_message_id: Uuid,
    pub source_id: String,
    pub idempotency_key: String,
    pub kind: SignalKind,
    pub severity: Severity,
    pub title: String,
    pub summary: String,
    #[serde(default = "empty_object")]
    pub facts: Value,
    /// Keep the original JSON until the server has checked for private fields.
    pub evidence: Value,
    pub occurred_at: Option<DateTime<Utc>>,
    pub external_id: Option<String>,
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub links: Vec<SignalLinkIn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SignalBatchOut {
    Receipts { delivery_id: Uuid, receipts: Vec<IngestReceipt> },
    Accepted { delivery_id: Uuid, accepted_count: usize },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntelSignal {
    pub id: Uuid,
    pub company_id: String,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    pub effective_audience: Audience,
    pub base_audience: Audience,
    /// Fixed to `PublicCleared` in cross-company public reads.
    pub source_default_audience: Audience,
    pub cleared_audience: Option<Audience>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub clearance_generation: i64,
    /// Review and storage metadata is absent from cross-company public reads.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub review: Option<IntelSignalReview>,
    pub state: SignalState,
    pub severity: Severity,
    pub kind: SignalKind,
    pub title: String,
    pub summary: String,
    pub facts: Value,
    pub subjects: Vec<SubjectRef>,
    pub evidence: Vec<Evidence>,
    pub confidence: f32,
    pub provenance: SignalProvenance,
    pub occurred_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntelSignalReview {
    pub projection_sha256: String,
    pub policy_version: i32,
    pub revision: i32,
    pub first_seen_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntelSignalRevision {
    pub revision: i32,
    pub projection_sha256: String,
    pub payload: Value,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntelSignalDetail {
    pub signal: IntelSignal,
    pub links: Vec<SignalLink>,
    pub revisions: Vec<IntelSignalRevision>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntelSignalQuery {
    pub source: Option<String>,
    pub kind: Option<SignalKind>,
    pub state: Option<SignalState>,
    pub severity_min: Option<Severity>,
    pub audience_at_most: Option<Audience>,
    pub persona: Option<Uuid>,
    pub project: Option<Uuid>,
    /// An exact subject value, or `scheme:value` to also constrain the scheme.
    pub subject: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub q: Option<String>,
    pub include_expired: bool,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntelSignalPage {
    pub items: Vec<IntelSignal>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalDecisionIn {
    pub action: SignalDecisionAction,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalClearanceIn {
    pub audience: Audience,
    pub revision: i32,
    pub policy_version: i32,
    pub source_default_audience: Audience,
    pub projection_sha256: String,
    pub rationale: String,
}

/// Generation-aware clearance. The legacy request remains valid on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClearSignalIn {
    #[serde(flatten)]
    pub reviewed: SignalClearanceIn,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_clearance_generation: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntelSourceStatus {
    pub id: String,
    pub company_id: String,
    pub policy_version: i32,
    pub default_audience: Audience,
    pub last_ingest_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub last_status: Option<IntelRunStatus>,
    pub last_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<Value>,
    pub stale: bool,
    pub stale_after_secs: u64,
}
