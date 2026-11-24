//! Mail and calendar contracts. Owner identity comes from authentication, never
//! from a user mutation payload. Calendar credentials and feed URLs stay out of these types.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MailTransportConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls_mode: Option<String>,
    pub folders: Vec<String>,
    pub export_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailAccount {
    pub id: Uuid,
    pub owner: String,
    pub label: String,
    pub provider: String,
    pub transport: String,
    pub address: String,
    pub config_json: MailTransportConfig,
    pub enabled: bool,
    pub status: String,
    pub sync_cursor_json: Value,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub message_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailAccountUpsert {
    pub label: String,
    pub provider: String,
    pub transport: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub config_json: MailTransportConfig,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailMessageIn {
    pub message_id_header: Option<String>,
    pub content_sha256: Option<String>,
    pub thread_key: String,
    pub folder: String,
    pub from_addr: String,
    pub from_name: String,
    pub from_domain: String,
    pub to_addrs: Vec<String>,
    pub cc_addrs: Vec<String>,
    pub subject: String,
    pub sent_at: Option<DateTime<Utc>>,
    pub received_at: Option<DateTime<Utc>>,
    pub snippet: String,
    pub body_text: String,
    pub has_attachments: bool,
    pub attachment_names: Vec<String>,
    pub raw_headers: BTreeMap<String, String>,
    pub size_bytes: i64,
    pub is_read: bool,
    /// Trusted worker storage reference; ignored on user ingest.
    pub raw_path: Option<String>,
    pub imap_location: Option<MailImapLocationIn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailImapLocationIn {
    pub folder: String,
    pub uidvalidity: i64,
    pub uid: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailIngestRequest {
    pub account_id: Uuid,
    pub messages: Vec<MailMessageIn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailIngestResult {
    pub inserted: usize,
    pub duplicates: usize,
    /// Newly inserted messages flagged as junk by the initial classifier.
    #[serde(default)]
    pub junk: usize,
}

/// First-time local imports can name an account instead of supplying its UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailNamedIngestRequest {
    pub account: String,
    pub provider: Option<String>,
    pub address: Option<String>,
    pub messages: Vec<MailMessageIn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailMessageSummary {
    pub id: Uuid,
    pub account_id: Uuid,
    pub provider: String,
    pub thread_key: String,
    pub folder: String,
    pub from_addr: String,
    pub from_name: String,
    pub from_domain: String,
    pub subject: String,
    pub sent_at: Option<DateTime<Utc>>,
    pub received_at: DateTime<Utc>,
    pub snippet: String,
    pub has_attachments: bool,
    pub is_read: bool,
    /// Effective values, after any user overrides.
    pub classification: String,
    pub is_junk: bool,
    pub priority: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailMessageDetail {
    #[serde(flatten)]
    pub message: MailMessageSummary,
    pub message_id_header: Option<String>,
    pub content_sha256: String,
    pub to_addrs: Vec<String>,
    pub cc_addrs: Vec<String>,
    pub body_text: String,
    pub attachment_names: Vec<String>,
    pub raw_headers: BTreeMap<String, String>,
    pub size_bytes: i64,
    pub ingested_at: DateTime<Utc>,
    pub analysis: MailAnalysisView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailAnalysisView {
    pub classification: String,
    pub is_junk: bool,
    pub junk_score: f64,
    pub priority: String,
    pub summary: String,
    pub entities: MailEntities,
    pub action_items: Vec<String>,
    pub topics: Vec<String>,
    pub user_override_classification: Option<String>,
    pub user_override_junk: Option<bool>,
    pub analyzer_kind: String,
    pub analyzer_model: Option<String>,
    pub analyzed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailEntities {
    pub vendor: Option<String>,
    pub amounts: Vec<MonetaryAmount>,
    pub invoice_ids: Vec<String>,
    pub deadlines: Vec<String>,
    pub people: Vec<String>,
    pub orgs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonetaryAmount {
    /// Exact normalized decimal (not floating point).
    pub amount: String,
    pub currency: String,
    pub raw: String,
}

/// Omitted fields leave the override unchanged; explicit null clears it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MailTriageOverride {
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub classification: Option<Option<String>>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub is_junk: Option<Option<bool>>,
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MailSearchQuery {
    pub account_id: Option<Uuid>,
    pub provider: Option<String>,
    pub classification: Option<String>,
    pub include_junk: bool,
    pub q: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MailBiOverview {
    pub total_count: i64,
    pub signal_count: i64,
    pub junk_count: i64,
    pub unread_count: i64,
    pub by_provider: BTreeMap<String, i64>,
    pub by_classification: BTreeMap<String, i64>,
    pub business_domains: Vec<MailBiDomainStat>,
    pub business_senders: Vec<MailBiDomainStat>,
    pub recurring_vendors: Vec<MailBiVendorSummary>,
    pub recent_amounts: Vec<MonetaryAmount>,
    pub open_action_items: Vec<MailBiActionItem>,
    pub unsubscribe_candidates: Vec<MailBiDomainStat>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailBiVendorSummary {
    pub vendor: String,
    pub message_count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailBiActionItem {
    pub message_id: Uuid,
    pub subject: String,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailBiDomainStat {
    pub key: String,
    pub message_count: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailBiDigest {
    pub id: Uuid,
    pub owner: String,
    pub period_kind: String,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub summary_markdown: String,
    pub metrics_json: Value,
    pub highlights_json: Value,
    pub generated_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarSource {
    pub id: Uuid,
    pub owner: String,
    pub label: String,
    pub provider: String,
    pub visibility: String,
    pub color: String,
    pub enabled: bool,
    pub status: String,
    pub sync_etag: Option<String>,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub event_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarSourceUpsert {
    pub label: String,
    pub provider: String,
    #[serde(default = "private")]
    pub visibility: String,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn private() -> String {
    "private".into()
}

fn default_color() -> String {
    "#4f8cff".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: Uuid,
    pub source_id: Uuid,
    pub owner: String,
    pub visibility: String,
    #[serde(flatten)]
    pub event: CalendarEventIn,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarAgendaResponse {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub events: Vec<CalendarEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarEventIn {
    pub uid: String,
    #[serde(default)]
    pub recurrence_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub organizer: String,
    #[serde(default)]
    pub attendees: Vec<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    #[serde(default = "utc")]
    pub timezone: String,
    #[serde(default)]
    pub all_day: bool,
    #[serde(default = "confirmed")]
    pub status: String,
    pub rrule: Option<String>,
    pub etag: Option<String>,
}

fn utc() -> String {
    "UTC".into()
}

fn confirmed() -> String {
    "confirmed".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarSyncResult {
    pub event_count: i64,
    pub last_sync_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn triage_distinguishes_omitted_cleared_and_false() {
        let wire = json!({"classification": null, "is_junk": false});
        let input: MailTriageOverride = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(input.classification, Some(None));
        assert_eq!(input.is_junk, Some(Some(false)));
        assert_eq!(serde_json::to_value(input).unwrap(), wire);
        assert_eq!(serde_json::to_value(MailTriageOverride::default()).unwrap(), json!({}));
    }
}
