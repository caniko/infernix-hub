//! Generic wire contracts for AI model providers, multi-account management,
//! quota/usage observation, activity analytics, routing, capture and exports.
//!
//! No provider-specific enum arms live here: Antigravity, Claude, OpenCode,
//! Spillway-compatible proxies, and future providers all flow through stable
//! string provider ids, capability flags, and typed command facets.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{AdapterMode, Verification};

// ── Capabilities & Provider Descriptor ──────────────────────────────────────

/// Kinds of evidence a provider adapter or collector can supply.
/// Supplying an evidence kind never by itself grants read access to sensitive
/// payloads (such as captured bodies or raw wire traffic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    AccountInventory,
    QuotaWindows,
    ResetGrants,
    RequestMetadata,
    TraceEdges,
    TransformedBodies,
    RawWire,
}

/// Operation-level capability flags advertised by a provider adapter.
/// The UI and API intersect adapter capabilities, account capabilities, caller
/// permissions, and current readiness before exposing or executing an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    // Quota facet
    RefreshQuota,
    RefreshGrants,
    ClaimResetGrant,
    StartRollingWindow,
    // Accounts facet
    SelectLocalAccount,
    SelectPersistentAccount,
    SetEnabled,
    SetPriority,
    SetWeight,
    SetCapacity,
    SetNote,
    // Credentials facet
    BeginOAuth,
    RenewLogin,
    RemoveLogin,
    ImportLogin,
    ExportLogin,
    RevokeGrant,
    // Key groups & models facet
    ManageKeyGroups,
    ManageKeys,
    ToggleKey,
    SetGroupEnabled,
    SetRequestIdentity,
    ConfigureModels,
    DiscoverModels,
    SetModelAliases,
    // Routing & proxy facet
    ReadProxyStatus,
    ControlProxy,
    ConfigureProxy,
    SetRoutePolicy,
    SetPoolOverride,
    MoveConversation,
    ReadClientWiring,
    ApplyClientWiring,
    UndoClientWiring,
    ManageClientKey,
    ManageLockouts,
    ReadProxyLogs,
    // Active probe facet
    RunCanary,
    RunThinkingTest,
    ReplayTextEval,
}

pub type ProviderCapabilities = BTreeSet<ProviderCapability>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealth {
    Healthy,
    #[serde(alias = "error")]
    Degraded,
    Offline,
    #[default]
    Unobserved,
}

/// Generic descriptor for any registered or observed model provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: String,
    pub label: String,
    /// Open family tag for grouping/styling (e.g. "claude", "antigravity", "opencode", "openai", "custom").
    pub family: String,
    pub mode: AdapterMode,
    pub verification: Verification,
    #[serde(default)]
    pub evidence_kinds: BTreeSet<EvidenceKind>,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    /// Shared router instance id when multiple providers sit behind one proxy.
    #[serde(default)]
    pub router_id: Option<String>,
    #[serde(default)]
    pub health: ProviderHealth,
    #[serde(default)]
    pub status_message: Option<String>,
    #[serde(default)]
    pub last_observed_at: Option<DateTime<Utc>>,
}

/// Summary row for `GET /v1/providers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSummary {
    pub descriptor: ProviderDescriptor,
    /// Counts are scoped to the reported router pool when known.
    pub account_count: usize,
    pub enabled_accounts: usize,
    pub active_account_id: Option<String>,
    pub preferred_account_id: Option<String>,
    pub next_pick_account_id: Option<String>,
    pub pools: Vec<PoolWindowSummary>,
    /// Retained account inventory, including accounts outside the router pool.
    pub accounts: Vec<ModelAccount>,
    /// Membership of the latest native proxy report; empty is a known empty pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_account_ids: Option<BTreeSet<String>>,
}

// ── Accounts & Quota Windows ────────────────────────────────────────────────

/// Display status precedence: Disabled → Limited (future cooldown) → NextPick → Error → Ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Disabled,
    Limited,
    NextPick,
    Error,
    #[default]
    Ready,
}

impl AccountStatus {
    /// Structured blocking errors outrank routing preference and quota cooldown.
    /// Keep the legacy ordering for soft probe failures.
    pub fn resolve_with_error(
        enabled: bool,
        cooldown_until: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
        is_next_pick: bool,
        probe_failed: bool,
        error: Option<&AccountError>,
    ) -> Self {
        if enabled && error.is_some_and(|error| error.blocks_routing(now)) {
            Self::Error
        } else {
            let soft_error = probe_failed || error.is_some_and(|error| error.kind == AccountErrorKind::ProbeFailed);
            Self::resolve(enabled, cooldown_until, now, is_next_pick && !soft_error, soft_error)
        }
    }

    /// Compute canonical display status from account state without conflating
    /// disabled, cooldown-limited, next-pick, error, and ready states.
    pub fn resolve(enabled: bool, cooldown_until: Option<DateTime<Utc>>, now: DateTime<Utc>, is_next_pick: bool, has_error: bool) -> Self {
        if !enabled {
            Self::Disabled
        } else if cooldown_until.is_some_and(|until| until > now) {
            Self::Limited
        } else if is_next_pick {
            Self::NextPick
        } else if has_error {
            Self::Error
        } else {
            Self::Ready
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountErrorKind {
    VerificationRequired,
    AccountSuspended,
    AuthExpired,
    BillingRequired,
    PermissionDenied,
    QuotaExhausted,
    UpstreamUnavailable,
    ProbeFailed,
    QuotaProbeUnsupported,
    Other,
}

impl AccountErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::VerificationRequired => "verification_required",
            Self::AccountSuspended => "account_suspended",
            Self::AuthExpired => "auth_expired",
            Self::BillingRequired => "billing_required",
            Self::PermissionDenied => "permission_denied",
            Self::QuotaExhausted => "quota_exhausted",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::ProbeFailed => "probe_failed",
            Self::QuotaProbeUnsupported => "quota_probe_unsupported",
            Self::Other => "other",
        }
    }

    pub fn from_str_lossy(value: &str) -> Self {
        match value {
            "verification_required" => Self::VerificationRequired,
            "account_suspended" => Self::AccountSuspended,
            "auth_expired" => Self::AuthExpired,
            "billing_required" => Self::BillingRequired,
            "permission_denied" => Self::PermissionDenied,
            "quota_exhausted" => Self::QuotaExhausted,
            "upstream_unavailable" => Self::UpstreamUnavailable,
            "probe_failed" => Self::ProbeFailed,
            "quota_probe_unsupported" => Self::QuotaProbeUnsupported,
            _ => Self::Other,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::VerificationRequired => "Verification required",
            Self::AccountSuspended => "Account suspended",
            Self::AuthExpired => "Authentication expired",
            Self::BillingRequired => "Billing required",
            Self::PermissionDenied => "Permission denied",
            Self::QuotaExhausted => "Quota exhausted",
            Self::UpstreamUnavailable => "Upstream unavailable",
            Self::ProbeFailed => "Quota probe failed",
            Self::QuotaProbeUnsupported => "Quota probe unsupported",
            Self::Other => "Provider error",
        }
    }

    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            Self::VerificationRequired | Self::AccountSuspended | Self::AuthExpired | Self::BillingRequired | Self::PermissionDenied
        )
    }

    pub fn is_action_required(&self) -> bool {
        self.is_blocking()
    }

    pub fn is_transient(&self) -> bool {
        matches!(self, Self::UpstreamUnavailable | Self::ProbeFailed | Self::Other)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountErrorSource {
    Inference,
    Stream,
    #[serde(rename = "oauth_refresh")]
    OAuthRefresh,
    QuotaProbe,
    Collector,
}

impl AccountErrorSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inference => "inference",
            Self::Stream => "stream",
            Self::OAuthRefresh => "oauth_refresh",
            Self::QuotaProbe => "quota_probe",
            Self::Collector => "collector",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountError {
    pub kind: AccountErrorKind,
    pub source: AccountErrorSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_url: Option<String>,
    pub occurred_at: DateTime<Utc>,
    #[serde(default)]
    pub consecutive_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until: Option<DateTime<Utc>>,
}

impl AccountError {
    pub fn blocks_routing(&self, now: DateTime<Utc>) -> bool {
        self.kind != AccountErrorKind::QuotaProbeUnsupported
            && (self.kind.is_blocking() || self.backoff_until.is_some_and(|until| until > now))
    }

    /// A zero count is also valid for an immediate error from an older publisher.
    pub fn is_notification_worthy(&self) -> bool {
        match self.kind {
            AccountErrorKind::QuotaExhausted | AccountErrorKind::QuotaProbeUnsupported => false,
            AccountErrorKind::UpstreamUnavailable => self.consecutive_count >= 3,
            AccountErrorKind::ProbeFailed | AccountErrorKind::Other => self.consecutive_count >= 2,
            _ => true,
        }
    }

    pub fn sanitized(mut self) -> Self {
        self.message = sanitize_provider_error(&self.message);
        self.provider_code = self.provider_code.map(|code| sanitize_provider_error(&code)).filter(|code| !code.is_empty());
        self.action_hint = self.action_hint.map(|hint| sanitize_provider_error(&hint)).filter(|hint| !hint.is_empty());
        // Verification links can contain credentials in paths, queries, or fragments.
        self.action_url = self.action_url.as_deref().and_then(|value| {
            let url = url::Url::parse(value).ok()?;
            (url.scheme() == "https").then(|| url.origin().ascii_serialization()).filter(|origin| origin.len() <= 240)
        });
        self
    }

    pub fn summary(&self) -> String {
        let message = self.message.split_whitespace().collect::<Vec<_>>().join(" ");
        if message.is_empty() { self.kind.label().into() } else { format!("{}: {message}", self.kind.label()) }
    }
}

/// Redact credentials and URL details before making a provider diagnostic visible.
pub fn sanitize_provider_error(message: &str) -> String {
    let clean: String = message.chars().filter(|c| !c.is_control() || c.is_whitespace()).collect();
    let lower = clean.to_ascii_lowercase();
    let field_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-');
    let value_end = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ',' | ';' | '}' | ']');
    let mut out = String::new();
    let mut at = 0;
    while at < clean.len() {
        let tail = &clean[at..];
        let folded = &lower[at..];
        let first = tail.chars().next().unwrap();
        let boundary = at == 0 || !clean.as_bytes()[at - 1].is_ascii_alphanumeric();
        if folded.starts_with("https://") || folded.starts_with("http://") {
            let n = tail.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>')).unwrap_or(tail.len());
            // Even a URL path can contain a credential; only publish the origin.
            let origin = url::Url::parse(&tail[..n]).ok().map(|url| url.origin().ascii_serialization());
            out.push_str(origin.as_deref().unwrap_or("[url]"));
            at += n;
        } else if matches!(first, '?' | '#') {
            // Also drop query/fragment from relative provider links.
            at += tail.find(char::is_whitespace).unwrap_or(tail.len());
        } else if boundary && ["aiza", "sk-", "ya29.", "eyj", "ihub_", "ghp_", "gho_"].iter().any(|p| folded.starts_with(p)) {
            at += tail.find(value_end).unwrap_or(tail.len());
            out.push_str("[redacted]");
        } else if boundary && field_char(first) {
            let n = tail.find(|c| !field_char(c)).unwrap_or(tail.len());
            let quoted_field = tail[n..].starts_with(['"', '\'']);
            let rest = tail[n..].trim_start_matches(['"', '\'']).trim_start_matches([' ', '\t']);
            let field = &folded[..n];
            let credential_header =
                matches!(field, "cookie" | "set-cookie" | "authorization" | "proxy-authorization" | "x-api-key" | "x-goog-api-key");
            let assigned = matches!(
                field,
                "key"
                    | "api_key"
                    | "apikey"
                    | "api-key"
                    | "token"
                    | "api_token"
                    | "auth_token"
                    | "access_token"
                    | "accesstoken"
                    | "refresh_token"
                    | "refreshtoken"
                    | "id_token"
                    | "idtoken"
                    | "client_secret"
                    | "clientsecret"
                    | "session_token"
                    | "sessiontoken"
                    | "secret"
                    | "password"
            );
            let value = if field == "bearer" && tail.len() > n {
                Some(rest.trim_start_matches(['=', ':']).trim_start())
            } else if assigned || credential_header {
                rest.strip_prefix(['=', ':']).map(|v| v.trim_start_matches([' ', '\t']))
            } else {
                None
            };
            if let Some(value) = value {
                let consumed = if credential_header && !quoted_field {
                    // A raw header consumes the entire line even if its first cookie is quoted.
                    value.find(['\r', '\n']).unwrap_or(value.len())
                } else if let Some(quote @ ('"' | '\'')) = value.chars().next() {
                    let mut escaped = false;
                    value[1..]
                        .char_indices()
                        .find_map(|(i, c)| {
                            if std::mem::take(&mut escaped) {
                                None
                            } else if c == '\\' {
                                escaped = true;
                                None
                            } else {
                                (c == quote).then_some(i + 2)
                            }
                        })
                        .unwrap_or(value.len())
                } else if credential_header {
                    // Commas and semicolons may separate cookies, not headers.
                    value.find(['\r', '\n']).unwrap_or(value.len())
                } else {
                    // An unquoted Authorization value can include the authentication scheme.
                    let value_tail = &lower[clean.len() - value.len()..];
                    let payload = ["bearer", "basic"].into_iter().find_map(|scheme| {
                        value_tail.strip_prefix(scheme).filter(|s| s.starts_with(char::is_whitespace)).map(str::trim_start)
                    });
                    payload.map_or_else(
                        || value.find(value_end).unwrap_or(value.len()),
                        |p| value.len() - p.len() + p.find(value_end).unwrap_or(p.len()),
                    )
                };
                at += tail.len() - value.len() + consumed;
                out.push_str("[redacted]");
            } else {
                out.push_str(&tail[..n]);
                at += n;
            }
        } else {
            out.push(first);
            at += first.len_utf8();
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect()
}

/// Separate lifecycle truth: catalogued, deployed, selected, and verified by observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    Catalogued,
    Deployed,
    Selected,
    Verified,
    #[default]
    Unobserved,
    Error,
}

/// Distinguishes measured zero from unavailable, stale last-good, never observed, or unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaObservationState {
    Measured,
    StaleLastGood,
    Unavailable,
    #[default]
    NeverObserved,
    Unsupported,
    ProbeFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindowKind {
    Session,
    Weekly,
    Monthly,
    Daily,
    ModelScoped,
    Spend,
    #[default]
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaUnit {
    #[default]
    Percent,
    Credits,
    Currency,
    Requests,
    Tokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResetCadence {
    Fixed,
    RollingOnUse,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    Allowed,
    Warning,
    Rejected,
    #[default]
    Unknown,
}

/// A single quota/allowance window observed on an account.
/// Window keys, labels, units, and cadences are data — never hardcoded to one provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub window_key: String,
    pub label: String,
    #[serde(default)]
    pub kind: QuotaWindowKind,
    #[serde(default)]
    pub unit: QuotaUnit,
    /// `None` when not observed — never coerced to 0.0.
    #[serde(default)]
    pub used_percent: Option<f64>,
    /// `None` when not observed — never coerced to 0.0.
    #[serde(default)]
    pub remaining_percent: Option<f64>,
    #[serde(default)]
    pub used_amount: Option<f64>,
    #[serde(default)]
    pub limit_amount: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub window_duration_mins: Option<u32>,
    #[serde(default)]
    pub cadence: ResetCadence,
    /// Optional model/bucket scope (e.g. "opus", "sonnet", "gemini-argon").
    #[serde(default)]
    pub scope_model: Option<String>,
    #[serde(default)]
    pub status: QuotaStatus,
    #[serde(default)]
    pub observation_state: QuotaObservationState,
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub error_reason: Option<String>,
}

fn window_horizon(key: &str) -> Option<(&str, u32)> {
    // Match whole keys or delimiter-separated suffixes, including multi-token horizons.
    for (token, minutes) in [
        ("five_hour", 300),
        ("five-hour", 300),
        ("seven_day", 10080),
        ("seven-day", 10080),
        ("session", 300),
        ("rolling", 300),
        ("quota", 300),
        ("5h", 300),
        ("daily", 1440),
        ("day", 1440),
        ("24h", 1440),
        ("1d", 1440),
        ("weekly", 10080),
        ("week", 10080),
        ("7d", 10080),
        ("monthly", 43200),
        ("month", 43200),
        ("30d", 43200),
    ] {
        if let Some(prefix) = key.strip_suffix(token) {
            if prefix.is_empty() {
                return Some(("", minutes));
            }
            if let Some(family) = prefix.strip_suffix(['-', '_']) {
                return Some((family, minutes));
            }
        }
    }
    None
}

/// Infer the horizon from explicit duration, calendar window kind, a key suffix, or session kind.
pub fn infer_window_duration_mins(window: &QuotaWindow) -> Option<u32> {
    window.window_duration_mins.filter(|&mins| mins > 0).or_else(|| match window.kind {
        QuotaWindowKind::Daily => Some(1440),
        QuotaWindowKind::Weekly => Some(10080),
        QuotaWindowKind::Monthly => Some(43200),
        _ => window_horizon(&window.window_key.to_ascii_lowercase())
            .map(|(_, mins)| mins)
            .or((window.kind == QuotaWindowKind::Session).then_some(300)),
    })
}

/// Return the scope prefix of a recognized horizon key, if it has one.
pub fn window_scope_family(window_key: &str) -> Option<String> {
    let key = window_key.to_ascii_lowercase();
    window_horizon(&key).filter(|(family, _)| !family.is_empty()).map(|(family, _)| family.to_owned())
}

fn raw_remaining_pct(window: &QuotaWindow) -> Option<f64> {
    window
        .remaining_percent
        .filter(|pct| pct.is_finite())
        .or_else(|| window.used_percent.filter(|pct| pct.is_finite()).map(|used| 100.0 - used))
        .map(|pct| pct.clamp(0.0, 100.0))
}

/// Whether a measured longer quota also constrains the shorter quota.
pub fn window_binds(longer: &QuotaWindow, shorter: &QuotaWindow) -> bool {
    longer.unit == QuotaUnit::Percent
        && shorter.unit == QuotaUnit::Percent
        && matches!(longer.observation_state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood)
        && raw_remaining_pct(longer).is_some()
        && matches!((infer_window_duration_mins(longer), infer_window_duration_mins(shorter)), (Some(l), Some(s)) if l > s)
        && window_scope_family(&longer.window_key) == window_scope_family(&shorter.window_key)
        && longer
            .scope_model
            .as_deref()
            .is_none_or(|model| shorter.scope_model.as_deref().is_some_and(|other| model.eq_ignore_ascii_case(other)))
}

/// Whether a compatible longer quota still constrains the shorter quota at `now`.
pub fn window_binds_at(longer: &QuotaWindow, shorter: &QuotaWindow, now: DateTime<Utc>) -> bool {
    longer.resets_at.is_none_or(|reset| reset > now) && window_binds(longer, shorter)
}

/// Percent remaining after applying all compatible, longer quota limits.
pub fn effective_remaining_percent(window: &QuotaWindow, all_windows: &[QuotaWindow]) -> Option<f64> {
    effective_remaining_percent_with(window, all_windows, |longer| window_binds(longer, window))
}

/// Percent remaining after applying compatible longer limits that have not reset.
pub fn effective_remaining_percent_at(window: &QuotaWindow, all_windows: &[QuotaWindow], now: DateTime<Utc>) -> Option<f64> {
    effective_remaining_percent_with(window, all_windows, |longer| window_binds_at(longer, window, now))
}

fn effective_remaining_percent_with(
    window: &QuotaWindow,
    all_windows: &[QuotaWindow],
    binds: impl Fn(&QuotaWindow) -> bool,
) -> Option<f64> {
    Some(all_windows.iter().filter(|longer| binds(longer)).filter_map(raw_remaining_pct).fold(raw_remaining_pct(window)?, f64::min))
}

/// Provider reset credit / grant status (e.g. Codex/Claude reset credits).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetGrant {
    pub grant_id: String,
    pub available_count: u32,
    pub applicable: bool,
    pub checked_at: DateTime<Utc>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub cleared_windows: Vec<String>,
    #[serde(default)]
    pub weekly_credit_back: bool,
    #[serde(default)]
    pub unavailable_reason: Option<String>,
}

/// Opt-in rolling-window starter configuration and status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RollingWindowStarterConfig {
    pub enabled: bool,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub stagger_mins: u32,
    /// Optional local active-hours start (0..=23).
    #[serde(default)]
    pub active_hours_start: Option<u8>,
    /// Optional local active-hours end (0..=23).
    #[serde(default)]
    pub active_hours_end: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RollingWindowStatus {
    pub enabled: bool,
    #[serde(default)]
    pub active_window_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub next_scheduled_start_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Logical account belonging to a model provider (subscription login or key binding).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAccount {
    pub id: String,
    pub provider_id: String,
    pub source_account_id: String,
    pub display_name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub masked_email: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    /// Relative plan capacity multiplier (e.g. 1.0, 5.0, 20.0); never confused with routing weight.
    #[serde(default)]
    pub tier_multiplier: Option<f64>,
    /// Advisory token-burn estimate; does not change the declared entitlement.
    #[serde(default)]
    pub observed_capacity_ratio: Option<f64>,
    #[serde(default)]
    pub status: AccountStatus,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub cooldown_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<AccountError>,
    /// When true, an absent `last_error` is explicit recovery, not unsupported reporting.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last_error_observed: bool,
    /// Currently selected/active on the local consumer.
    #[serde(default)]
    pub is_active: bool,
    /// Explicit operator preference override.
    #[serde(default)]
    pub is_preferred: bool,
    /// Scheduler's computed next pick.
    #[serde(default)]
    pub is_next_pick: bool,
    #[serde(default)]
    pub enrollment_state: EnrollmentState,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub unix_user: Option<String>,
    #[serde(default)]
    pub credential_fingerprint: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    /// Observed success/failure counters (especially useful for API-key accounts without quota).
    #[serde(default)]
    pub success_count: u64,
    #[serde(default)]
    pub failure_count: u64,
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    #[serde(default)]
    pub reset_grant: Option<ResetGrant>,
    #[serde(default)]
    pub rolling_starter: Option<RollingWindowStatus>,
}

fn default_true() -> bool {
    true
}

fn default_weight() -> u32 {
    1
}

/// Mask an email address for UI presentation (presentation masking only, not storage redaction).
pub fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "***".to_string();
    };
    let mut chars = local.chars();
    let head = chars.next().map(|c| c.to_string()).unwrap_or_default();
    let tail = if local.chars().count() > 2 { local.chars().last().map(|c| c.to_string()).unwrap_or_default() } else { String::new() };
    format!("{head}***{tail}@{domain}")
}

// ── Quota History, Past Windows, Forecast & Pool ────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSample {
    pub id: i64,
    pub provider_id: String,
    pub account_id: String,
    pub window_key: String,
    #[serde(default)]
    pub scope_model: Option<String>,
    pub label: String,
    pub kind: QuotaWindowKind,
    pub unit: QuotaUnit,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub used_amount: Option<f64>,
    pub limit_amount: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub window_duration_mins: Option<u32>,
    pub cadence: ResetCadence,
    pub status: QuotaStatus,
    pub observation_state: QuotaObservationState,
    pub observed_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub stale: bool,
    pub error_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSampleIn {
    pub account_id: String,
    pub window: QuotaWindow,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PastWindowSummary {
    pub account_id: String,
    pub window_key: String,
    #[serde(default)]
    pub scope_model: Option<String>,
    pub label: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub peak_used_percent: Option<f64>,
    pub min_remaining_percent: Option<f64>,
    pub exhausted_at: Option<DateTime<Utc>>,
    pub early_replaced: bool,
    pub first_observed_at: DateTime<Utc>,
    pub last_observed_at: DateTime<Utc>,
    pub sample_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastPoint {
    pub at: DateTime<Utc>,
    pub used_percent: f64,
    pub remaining_percent: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaForecast {
    pub window_key: String,
    pub account_id: Option<String>,
    #[serde(default)]
    pub scope_model: Option<String>,
    /// Percentage points drained per hour (estimated over `lookback_hours`).
    pub drain_pct_per_hour: f64,
    pub projected_exhaustion_at: Option<DateTime<Utc>>,
    pub resets_before_exhaustion: bool,
    pub lookback_hours: u32,
    pub horizon_hours: u32,
    pub points: Vec<ForecastPoint>,
    /// Always true: forecasts are computed estimates, never provider guarantees.
    pub is_estimate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolWindowSummary {
    pub provider_id: String,
    pub window_key: String,
    #[serde(default)]
    pub scope_model: Option<String>,
    pub label: String,
    pub kind: QuotaWindowKind,
    pub eligible_accounts: usize,
    pub included_accounts: usize,
    /// Weighted average across compatible eligible windows; `None` when none observed.
    pub weighted_used_percent: Option<f64>,
    pub weighted_remaining_percent: Option<f64>,
    #[serde(default)]
    pub remaining_capacity_units: Option<f64>,
    #[serde(default)]
    pub total_capacity_units: Option<f64>,
    /// Explains what weights were used (e.g. "plan_multiplier", "equal_entitlements").
    pub capacity_basis: String,
    pub next_reset_at: Option<DateTime<Utc>>,
    /// Duration shared by the included windows, when known.
    #[serde(default)]
    pub window_seconds: Option<u64>,
    pub forecast: Option<QuotaForecast>,
    pub is_estimate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct QuotaQuery {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub window_key: Option<String>,
    #[serde(default)]
    pub range_hours: Option<u32>,
    #[serde(default)]
    pub lookback_hours: Option<u32>,
    #[serde(default)]
    pub horizon_hours: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderQuotaReport {
    pub provider: ProviderDescriptor,
    pub accounts: Vec<ModelAccount>,
    pub pools: Vec<PoolWindowSummary>,
    pub samples: Vec<QuotaSample>,
    /// Older samples in the requested range were omitted by the history cap.
    #[serde(default)]
    pub history_truncated: bool,
    pub past_windows: Vec<PastWindowSummary>,
    pub forecasts: Vec<QuotaForecast>,
    pub as_of: DateTime<Utc>,
}

// ── Key Groups, Models & OAuth Sessions ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestIdentityMode {
    #[default]
    Api,
    AutoClient,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaskedApiKey {
    pub id: String,
    pub label: String,
    /// Last 4 characters or safe prefix/suffix hint; never the raw key.
    pub key_hint: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub success_count: u64,
    #[serde(default)]
    pub failure_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAlias {
    pub alias: String,
    pub target_model_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub family: Option<String>,
    pub provider_id: String,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub supports_thinking: Option<bool>,
    #[serde(default)]
    pub discovered_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyGroup {
    pub id: String,
    pub provider_id: String,
    /// e.g. "claude", "codex", "gemini", "vertex", "gemini_interactions", "xai", "meta", "openai_compat"
    pub family: String,
    pub name: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub request_identity: RequestIdentityMode,
    #[serde(default)]
    pub keys: Vec<MaskedApiKey>,
    #[serde(default)]
    pub configured_models: Vec<String>,
    #[serde(default)]
    pub discovered_models: Vec<DiscoveredModel>,
    #[serde(default)]
    pub aliases: Vec<ModelAlias>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthLoginState {
    pub session_id: String,
    pub provider_id: String,
    pub account_hint: Option<String>,
    pub state: String,
    pub authorize_url: Option<String>,
    pub message: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

// ── Typed Provider Commands & Operations ────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum QuotaCommand {
    RefreshQuota { account_id: Option<String> },
    RefreshGrants { account_id: Option<String> },
    ClaimResetGrant { account_id: String, grant_id: String },
    ConfigureRollingStarter { config: RollingWindowStarterConfig },
    TriggerRollingStart { account_id: String, model_id: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AccountCommand {
    SelectLocalAccount { account_id: String, host: Option<String>, unix_user: Option<String> },
    SelectPersistentAccount { account_id: String, scope: String, target: Option<String> },
    SetEnabled { account_id: String, enabled: bool },
    SetPriority { account_id: String, priority: i32 },
    SetWeight { account_id: String, weight: u32 },
    SetCapacity { account_id: String, plan: Option<String>, tier_multiplier: Option<f64> },
    SetNote { account_id: String, note: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CredentialCommand {
    BeginOAuth { account_label: Option<String>, host: Option<String>, unix_user: Option<String> },
    RenewLogin { account_id: Option<String> },
    RemoveLogin { account_id: String },
    ImportLogin { label: Option<String>, source_hint: String },
    ExportLogin { account_id: String },
    RevokeGrant { account_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum KeyGroupCommand {
    UpsertGroup {
        group_id: Option<String>,
        family: String,
        name: String,
        base_url: Option<String>,
        enabled: bool,
        request_identity: RequestIdentityMode,
        configured_models: Vec<String>,
        aliases: Vec<ModelAlias>,
    },
    DeleteGroup {
        group_id: String,
    },
    AddKey {
        group_id: String,
        label: String,
        /// Secret reference or masked token handle — raw keys stay with the owner when possible.
        key_ref: String,
    },
    ToggleKey {
        group_id: String,
        key_id: String,
        enabled: bool,
    },
    RemoveKey {
        group_id: String,
        key_id: String,
    },
    SetGroupEnabled {
        group_id: String,
        enabled: bool,
    },
    SetRequestIdentity {
        group_id: String,
        mode: RequestIdentityMode,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ModelCommand {
    DiscoverModels { group_id: Option<String> },
    SetModels { group_id: String, models: Vec<String> },
    SetAliases { group_id: String, aliases: Vec<ModelAlias> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProxyAction {
    #[default]
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RoutingCommand {
    ControlProxy { action: ProxyAction },
    UpdateProxyConfig { port: Option<u16>, lan_bind: Option<bool>, auto_start: Option<bool>, debug_logs: Option<bool> },
    SetRoutePolicy { policy: RoutingPolicy },
    SetPoolOverride { account_id: Option<String> },
    MoveConversation { session_id: String, target_account_id: String, include_subagents: bool },
    ApplyClientWiring { client_id: String },
    UndoClientWiring { client_id: String },
    ReapplyClientWiring { client_id: String },
    RegenerateClientKey,
    UnlockOutsideTool { lockout_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ProbeCommand {
    RunCanary {
        model_id: String,
        effort: Option<String>,
        account_id: Option<String>,
    },
    ConfigureCanarySchedule {
        schedule: CanarySchedule,
    },
    RunThinkingTest {
        model_id: String,
        effort: String,
        compare_model_id: Option<String>,
        compare_effort: Option<String>,
        sample_count: u32,
    },
    ReplayTextEval {
        case_id: Uuid,
        account_id: Option<String>,
        model_override: Option<String>,
        effort_override: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "facet", content = "payload", rename_all = "snake_case")]
pub enum ProviderCommand {
    Quota(QuotaCommand),
    Account(AccountCommand),
    Credential(CredentialCommand),
    KeyGroup(KeyGroupCommand),
    Model(ModelCommand),
    Routing(RoutingCommand),
    Probe(ProbeCommand),
}

impl ProviderCommand {
    pub fn required_capability(&self) -> ProviderCapability {
        match self {
            Self::Quota(QuotaCommand::RefreshQuota { .. }) => ProviderCapability::RefreshQuota,
            Self::Quota(QuotaCommand::RefreshGrants { .. }) => ProviderCapability::RefreshGrants,
            Self::Quota(QuotaCommand::ClaimResetGrant { .. }) => ProviderCapability::ClaimResetGrant,
            Self::Quota(QuotaCommand::ConfigureRollingStarter { .. } | QuotaCommand::TriggerRollingStart { .. }) => {
                ProviderCapability::StartRollingWindow
            }
            Self::Account(AccountCommand::SelectLocalAccount { .. }) => ProviderCapability::SelectLocalAccount,
            Self::Account(AccountCommand::SelectPersistentAccount { .. }) => ProviderCapability::SelectPersistentAccount,
            Self::Account(AccountCommand::SetEnabled { .. }) => ProviderCapability::SetEnabled,
            Self::Account(AccountCommand::SetPriority { .. }) => ProviderCapability::SetPriority,
            Self::Account(AccountCommand::SetWeight { .. }) => ProviderCapability::SetWeight,
            Self::Account(AccountCommand::SetCapacity { .. }) => ProviderCapability::SetCapacity,
            Self::Account(AccountCommand::SetNote { .. }) => ProviderCapability::SetNote,
            Self::Credential(CredentialCommand::BeginOAuth { .. }) => ProviderCapability::BeginOAuth,
            Self::Credential(CredentialCommand::RenewLogin { .. }) => ProviderCapability::RenewLogin,
            Self::Credential(CredentialCommand::RemoveLogin { .. }) => ProviderCapability::RemoveLogin,
            Self::Credential(CredentialCommand::ImportLogin { .. }) => ProviderCapability::ImportLogin,
            Self::Credential(CredentialCommand::ExportLogin { .. }) => ProviderCapability::ExportLogin,
            Self::Credential(CredentialCommand::RevokeGrant { .. }) => ProviderCapability::RevokeGrant,
            Self::KeyGroup(KeyGroupCommand::UpsertGroup { .. } | KeyGroupCommand::DeleteGroup { .. }) => {
                ProviderCapability::ManageKeyGroups
            }
            Self::KeyGroup(KeyGroupCommand::AddKey { .. } | KeyGroupCommand::RemoveKey { .. }) => ProviderCapability::ManageKeys,
            Self::KeyGroup(KeyGroupCommand::ToggleKey { .. }) => ProviderCapability::ToggleKey,
            Self::KeyGroup(KeyGroupCommand::SetGroupEnabled { .. }) => ProviderCapability::SetGroupEnabled,
            Self::KeyGroup(KeyGroupCommand::SetRequestIdentity { .. }) => ProviderCapability::SetRequestIdentity,
            Self::Model(ModelCommand::DiscoverModels { .. }) => ProviderCapability::DiscoverModels,
            Self::Model(ModelCommand::SetModels { .. }) => ProviderCapability::ConfigureModels,
            Self::Model(ModelCommand::SetAliases { .. }) => ProviderCapability::SetModelAliases,
            Self::Routing(RoutingCommand::ControlProxy { .. }) => ProviderCapability::ControlProxy,
            Self::Routing(RoutingCommand::UpdateProxyConfig { .. }) => ProviderCapability::ConfigureProxy,
            Self::Routing(RoutingCommand::SetRoutePolicy { .. }) => ProviderCapability::SetRoutePolicy,
            Self::Routing(RoutingCommand::SetPoolOverride { .. }) => ProviderCapability::SetPoolOverride,
            Self::Routing(RoutingCommand::MoveConversation { .. }) => ProviderCapability::MoveConversation,
            Self::Routing(RoutingCommand::ApplyClientWiring { .. } | RoutingCommand::ReapplyClientWiring { .. }) => {
                ProviderCapability::ApplyClientWiring
            }
            Self::Routing(RoutingCommand::UndoClientWiring { .. }) => ProviderCapability::UndoClientWiring,
            Self::Routing(RoutingCommand::RegenerateClientKey) => ProviderCapability::ManageClientKey,
            Self::Routing(RoutingCommand::UnlockOutsideTool { .. }) => ProviderCapability::ManageLockouts,
            Self::Probe(ProbeCommand::RunCanary { .. } | ProbeCommand::ConfigureCanarySchedule { .. }) => ProviderCapability::RunCanary,
            Self::Probe(ProbeCommand::RunThinkingTest { .. }) => ProviderCapability::RunThinkingTest,
            Self::Probe(ProbeCommand::ReplayTextEval { .. }) => ProviderCapability::ReplayTextEval,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Quota(QuotaCommand::RefreshQuota { .. }) => "quota.refresh",
            Self::Quota(QuotaCommand::RefreshGrants { .. }) => "quota.refresh_grants",
            Self::Quota(QuotaCommand::ClaimResetGrant { .. }) => "quota.claim_reset_grant",
            Self::Quota(QuotaCommand::ConfigureRollingStarter { .. }) => "quota.configure_rolling_starter",
            Self::Quota(QuotaCommand::TriggerRollingStart { .. }) => "quota.trigger_rolling_start",
            Self::Account(AccountCommand::SelectLocalAccount { .. }) => "account.select_local",
            Self::Account(AccountCommand::SelectPersistentAccount { .. }) => "account.select_persistent",
            Self::Account(AccountCommand::SetEnabled { .. }) => "account.set_enabled",
            Self::Account(AccountCommand::SetPriority { .. }) => "account.set_priority",
            Self::Account(AccountCommand::SetWeight { .. }) => "account.set_weight",
            Self::Account(AccountCommand::SetCapacity { .. }) => "account.set_capacity",
            Self::Account(AccountCommand::SetNote { .. }) => "account.set_note",
            Self::Credential(CredentialCommand::BeginOAuth { .. }) => "credential.begin_oauth",
            Self::Credential(CredentialCommand::RenewLogin { .. }) => "credential.renew",
            Self::Credential(CredentialCommand::RemoveLogin { .. }) => "credential.remove",
            Self::Credential(CredentialCommand::ImportLogin { .. }) => "credential.import",
            Self::Credential(CredentialCommand::ExportLogin { .. }) => "credential.export",
            Self::Credential(CredentialCommand::RevokeGrant { .. }) => "credential.revoke_grant",
            Self::KeyGroup(KeyGroupCommand::UpsertGroup { .. }) => "key_group.upsert",
            Self::KeyGroup(KeyGroupCommand::DeleteGroup { .. }) => "key_group.delete",
            Self::KeyGroup(KeyGroupCommand::AddKey { .. }) => "key_group.add_key",
            Self::KeyGroup(KeyGroupCommand::ToggleKey { .. }) => "key_group.toggle_key",
            Self::KeyGroup(KeyGroupCommand::RemoveKey { .. }) => "key_group.remove_key",
            Self::KeyGroup(KeyGroupCommand::SetGroupEnabled { .. }) => "key_group.set_enabled",
            Self::KeyGroup(KeyGroupCommand::SetRequestIdentity { .. }) => "key_group.set_request_identity",
            Self::Model(ModelCommand::DiscoverModels { .. }) => "model.discover",
            Self::Model(ModelCommand::SetModels { .. }) => "model.set_models",
            Self::Model(ModelCommand::SetAliases { .. }) => "model.set_aliases",
            Self::Routing(RoutingCommand::ControlProxy { .. }) => "routing.control_proxy",
            Self::Routing(RoutingCommand::UpdateProxyConfig { .. }) => "routing.update_proxy_config",
            Self::Routing(RoutingCommand::SetRoutePolicy { .. }) => "routing.set_route_policy",
            Self::Routing(RoutingCommand::SetPoolOverride { .. }) => "routing.set_pool_override",
            Self::Routing(RoutingCommand::MoveConversation { .. }) => "routing.move_conversation",
            Self::Routing(RoutingCommand::ApplyClientWiring { .. }) => "routing.apply_client_wiring",
            Self::Routing(RoutingCommand::UndoClientWiring { .. }) => "routing.undo_client_wiring",
            Self::Routing(RoutingCommand::ReapplyClientWiring { .. }) => "routing.reapply_client_wiring",
            Self::Routing(RoutingCommand::RegenerateClientKey) => "routing.regenerate_client_key",
            Self::Routing(RoutingCommand::UnlockOutsideTool { .. }) => "routing.unlock_outside_tool",
            Self::Probe(ProbeCommand::RunCanary { .. }) => "probe.run_canary",
            Self::Probe(ProbeCommand::ConfigureCanarySchedule { .. }) => "probe.configure_canary_schedule",
            Self::Probe(ProbeCommand::RunThinkingTest { .. }) => "probe.run_thinking_test",
            Self::Probe(ProbeCommand::ReplayTextEval { .. }) => "probe.replay_text_eval",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    #[default]
    Pending,
    Dispatched,
    AwaitingOperator,
    Succeeded,
    Failed,
    Ambiguous,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitOperationIn {
    pub idempotency_key: String,
    #[serde(default)]
    pub expected_revision: Option<i64>,
    #[serde(default)]
    pub target_account_id: Option<String>,
    #[serde(default)]
    pub target_host: Option<String>,
    #[serde(default)]
    pub target_user: Option<String>,
    pub command: ProviderCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderOperation {
    pub id: Uuid,
    pub provider_id: String,
    pub actor: String,
    pub idempotency_key: String,
    pub command_kind: String,
    pub command: ProviderCommand,
    pub target_account_id: Option<String>,
    pub target_host: Option<String>,
    pub target_user: Option<String>,
    pub expected_revision: Option<i64>,
    pub status: OperationStatus,
    pub receipt: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimOperationsIn {
    pub host: String,
    pub unix_user: String,
    #[serde(default)]
    pub provider_ids: Vec<String>,
    #[serde(default = "default_claim_limit")]
    pub limit: u32,
}

fn default_claim_limit() -> u32 {
    10
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationReceiptIn {
    pub status: OperationStatus,
    #[serde(default)]
    pub receipt: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

// ── Observation Batch Ingestion ─────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationBatch {
    pub provider_id: String,
    pub provider_label: String,
    pub provider_family: String,
    pub source_host: String,
    pub source_user: String,
    /// Producer identity within the source host and user. Legacy batches cannot prune inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_publisher: Option<String>,
    pub collected_at: DateTime<Utc>,
    /// Includes all accounts owned by this publisher, even when none remain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub account_inventory_complete: bool,
    #[serde(default)]
    pub health: ProviderHealth,
    #[serde(default)]
    pub status_message: Option<String>,
    #[serde(default)]
    pub router_id: Option<String>,
    #[serde(default)]
    pub capabilities: ProviderCapabilities,
    #[serde(default)]
    pub evidence_kinds: BTreeSet<EvidenceKind>,
    #[serde(default)]
    pub accounts: Vec<ModelAccount>,
    #[serde(default)]
    pub quota_samples: Vec<QuotaSampleIn>,
    #[serde(default)]
    pub reset_grants: Vec<(String, ResetGrant)>,
    #[serde(default)]
    pub key_groups: Vec<KeyGroup>,
    #[serde(default)]
    pub models: Vec<DiscoveredModel>,
    #[serde(default)]
    pub routing_status: Option<RoutingView>,
    #[serde(default)]
    pub proxy_logs: Vec<ProxyLogEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationAck {
    pub provider_id: String,
    pub accounts_upserted: usize,
    pub samples_inserted: usize,
    pub grants_upserted: usize,
    pub recorded_at: DateTime<Utc>,
}

// ── Routing & Proxy Control Plane ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouteStrategy {
    #[default]
    CapacityBalanced,
    SoonestReset,
    FillFirst,
    RoundRobin,
    WeightedRoundRobin,
}

#[cfg(test)]
#[test]
fn capacity_balanced_is_the_default_route_strategy() {
    assert_eq!(serde_json::to_value(RoutingPolicy::default()).unwrap()["strategy"], "capacity_balanced");
    assert_eq!(serde_json::from_str::<RouteStrategy>("\"capacity_balanced\"").unwrap(), RouteStrategy::CapacityBalanced);
    assert_eq!(serde_json::from_str::<RoutingPolicy>("{}").unwrap(), RoutingPolicy::default());
    let explicit: RoutingPolicy = serde_json::from_value(serde_json::json!({"strategy": "soonest_reset"})).unwrap();
    assert_eq!(explicit.strategy, RouteStrategy::SoonestReset);
}

#[cfg(test)]
#[test]
fn reported_quota_binds_to_its_weekly_window() {
    let window = |key| {
        serde_json::from_value::<QuotaWindow>(serde_json::json!({
            "window_key": key, "label": key, "remaining_percent": 25.0, "observation_state": "measured"
        }))
        .unwrap()
    };
    assert_eq!(window_scope_family("3p-quota").as_deref(), Some("3p"));
    assert!(window_binds(&window("3p-weekly"), &window("3p-quota")));
}

#[test]
fn session_kind_supplies_duration_when_key_has_no_horizon() {
    let window: QuotaWindow = serde_json::from_value(serde_json::json!({
        "window_key": "primary_window", "label": "Primary", "kind": "session"
    }))
    .unwrap();
    assert_eq!(infer_window_duration_mins(&window), Some(300));
    assert_eq!(infer_window_duration_mins(&QuotaWindow { window_key: "weekly".into(), ..window }), Some(10080));
}

#[test]
fn binding_windows_respect_family_scope_and_measurement() {
    let window = |key| {
        serde_json::from_value::<QuotaWindow>(serde_json::json!({
            "window_key": key, "label": key, "remaining_percent": 25.0, "observation_state": "measured"
        }))
        .unwrap()
    };
    for (session, weekly) in
        [("3p-5h", "3p-weekly"), ("gemini-5h", "gemini-weekly"), ("claude_five_hour", "claude_seven_day"), ("five_hour", "seven_day")]
    {
        assert!(window_binds(&window(weekly), &window(session)));
        assert!(!window_binds(&window(session), &window(weekly)));
    }
    assert!(!window_binds(&window("gemini-weekly"), &window("3p-5h")));
    assert_eq!(window_scope_family("claude_seven_day").as_deref(), Some("claude"));
    assert_eq!(window_scope_family("seven_day"), None);
    let rolling = window("go_rolling");
    assert_eq!(window_scope_family(&rolling.window_key).as_deref(), Some("go"));
    assert_eq!(infer_window_duration_mins(&rolling), Some(300));
    assert!(window_binds(&window("go_weekly"), &rolling));
    assert_eq!(infer_window_duration_mins(&QuotaWindow { window_duration_mins: Some(180), ..rolling }), Some(180));

    let mut weekly = window("seven_day");
    let mut session = window("five_hour");
    session.remaining_percent = None;
    session.used_percent = Some(-1.0);
    weekly.scope_model = Some("Opus".into());
    assert!(!window_binds(&weekly, &session));
    session.scope_model = Some("opus".into());
    assert!(window_binds(&weekly, &session));
    weekly.scope_model = None;
    weekly.observation_state = QuotaObservationState::StaleLastGood;
    assert_eq!(effective_remaining_percent(&session, &[weekly.clone()]), Some(25.0));
    weekly.observation_state = QuotaObservationState::ProbeFailed;
    assert_eq!(effective_remaining_percent(&session, &[weekly]), Some(100.0));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyOrder {
    #[default]
    KeysLast,
    KeysFirst,
    SubscriptionsOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingPolicy {
    #[serde(default)]
    pub strategy: RouteStrategy,
    #[serde(default)]
    pub api_key_order: ApiKeyOrder,
    #[serde(default = "default_affinity_secs")]
    pub session_affinity_secs: u64,
    #[serde(default)]
    pub preferred_account_id: Option<String>,
    #[serde(default = "default_cooldown_secs")]
    pub failover_cooldown_secs: u64,
    #[serde(default = "default_true")]
    pub switch_on_server_error: bool,
    #[serde(default)]
    pub near_reset_wait_secs: u64,
    #[serde(default = "default_first_byte_secs")]
    pub first_byte_timeout_secs: u64,
}

fn default_affinity_secs() -> u64 {
    1800
}
fn default_cooldown_secs() -> u64 {
    120
}
fn default_first_byte_secs() -> u64 {
    45
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        Self {
            strategy: RouteStrategy::default(),
            api_key_order: ApiKeyOrder::KeysLast,
            session_affinity_secs: default_affinity_secs(),
            preferred_account_id: None,
            failover_cooldown_secs: default_cooldown_secs(),
            switch_on_server_error: default_true(),
            near_reset_wait_secs: 0,
            first_byte_timeout_secs: default_first_byte_secs(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyRuntimeStatus {
    pub router_id: String,
    pub running: bool,
    pub endpoint: Option<String>,
    pub port: Option<u16>,
    pub lan_bind: bool,
    pub auto_start: bool,
    pub debug_logs: bool,
    pub build_version: Option<String>,
    /// Masked hint of the client inference key; never the full key in summary reads.
    pub client_key_hint: Option<String>,
    pub shared_profile_owner: Option<String>,
    pub crash_restarts: u32,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClientWiringState {
    Applied,
    Outdated,
    #[default]
    NotApplied,
    ExternallyManaged,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientWiringStatus {
    pub client_id: String,
    pub display_name: String,
    pub state: ClientWiringState,
    pub backup_available: bool,
    pub managed_by: Option<String>,
    pub detail: Option<String>,
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyLockout {
    pub id: String,
    pub tool_name: String,
    pub reason: String,
    pub locked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyLogEntry {
    pub at: DateTime<Utc>,
    pub stream: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingView {
    pub provider_id: String,
    pub router_id: Option<String>,
    pub revision: i64,
    pub runtime: Option<ProxyRuntimeStatus>,
    pub policy: RoutingPolicy,
    pub clients: Vec<ClientWiringStatus>,
    pub lockouts: Vec<ProxyLockout>,
    pub recent_logs: Vec<ProxyLogEntry>,
    pub rolling_starter: RollingWindowStarterConfig,
}

// ── Activity, Traces, Drift, Totals, Thinking & Evals ───────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    #[default]
    Proxy,
    LocalImport,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPurpose {
    #[default]
    Ordinary,
    WindowStarter,
    Canary,
    ThinkingTest,
    EvalReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    #[default]
    None,
    MetadataOnly,
    Transformed,
    RawWire,
    Truncated,
    Evicted,
    Dropped,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TraceEdgeKind {
    #[default]
    Subagent,
    Fork,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityAttemptIn {
    pub source_attempt_id: String,
    pub attempt_index: i32,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub moved_from_account_id: Option<String>,
    pub requested_model: String,
    pub effective_model: String,
    #[serde(default)]
    pub effort: Option<String>,
    pub status_code: Option<i32>,
    pub outcome: String,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<i64>,
    #[serde(default)]
    pub ttft_ms: Option<i64>,
    /// Total input tokens (provider-inclusive; cache_read is a subset, never added on top).
    #[serde(default)]
    pub input_tokens: i64,
    /// Total output tokens (reasoning/thinking is a subset when provider-inclusive).
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
    #[serde(default)]
    pub thinking_tokens: i64,
    #[serde(default)]
    pub fast_mode: bool,
    #[serde(default)]
    pub capture_state: CaptureState,
    #[serde(default)]
    pub captured_body: Option<Value>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityExchangeIn {
    /// Stable source-scoped id so local imports and proxy events reconcile without double-counting.
    pub source_exchange_id: String,
    pub source: ActivitySource,
    #[serde(default)]
    pub purpose: ActivityPurpose,
    pub client: String,
    #[serde(default)]
    pub client_version: Option<String>,
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub parent_edge_kind: Option<TraceEdgeKind>,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub requested_model: String,
    pub effective_model: String,
    #[serde(default)]
    pub effort: Option<String>,
    pub outcome: String,
    pub status_code: Option<i32>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<i64>,
    #[serde(default)]
    pub ttft_ms: Option<i64>,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
    #[serde(default)]
    pub thinking_tokens: i64,
    #[serde(default)]
    pub fast_mode: bool,
    #[serde(default)]
    pub capture_state: CaptureState,
    #[serde(default)]
    pub searchable_text: Option<String>,
    pub occurred_at: DateTime<Utc>,
    #[serde(default)]
    pub attempts: Vec<ActivityAttemptIn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityIngestBatch {
    pub source_host: String,
    pub source_user: String,
    pub exchanges: Vec<ActivityExchangeIn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityIngestAck {
    pub accepted: usize,
    pub deduplicated: usize,
    pub recorded_at: DateTime<Utc>,
}

/// Shared filter/scope query for all 7 Activity tabs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ActivityScope {
    #[serde(default)]
    pub from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub to: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tz_offset_mins: Option<i32>,
    #[serde(default)]
    pub source: Option<ActivitySource>,
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub baseline_key: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPriceEntry {
    pub model_pattern: String,
    pub display_name: String,
    pub effective_from: String,
    pub currency: String,
    pub input_per_m: f64,
    pub output_per_m: f64,
    pub cache_read_per_m: f64,
    pub cache_write_5m_per_m: f64,
    pub source_note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityTrendPoint {
    pub bucket_start: DateTime<Utc>,
    pub label: String,
    pub requests: u64,
    pub failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cache_hit_rate: f64,
    /// `None` when the bucket has no input tokens. The numeric rate is retained for older clients.
    #[serde(default)]
    pub cache_hit_rate_observed: Option<f64>,
    pub thinking_tokens: u64,
    pub api_equivalent_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeatmapCell {
    /// Day of week 0 (Mon) .. 6 (Sun).
    pub day_of_week: u8,
    /// Local hour 0..23.
    pub hour: u8,
    pub requests: u64,
    pub tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_hit_rate: f64,
    pub api_equivalent_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RhythmBucket {
    pub hour: u8,
    pub requests: u64,
    #[serde(default)]
    pub failures: u64,
    pub tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_hit_rate: f64,
    pub api_equivalent_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakdownRow {
    pub key: String,
    pub label: String,
    pub requests: u64,
    pub failures: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cache_hit_rate: f64,
    pub thinking_tokens: u64,
    pub api_equivalent_cost_usd: f64,
    pub unpriced_requests: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardTile {
    pub id: String,
    pub kind: String,
    pub metric: String,
    pub span: u8,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityOverview {
    /// API-equivalent list-price estimate (explicitly not invoiced subscription spend).
    pub api_equivalent_cost_usd: f64,
    pub unpriced_requests: u64,
    pub unpriced_models: Vec<String>,
    pub price_as_of: String,
    pub cache_write_assumption: String,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub thinking_tokens: u64,
    pub requests: u64,
    pub failures: u64,
    pub cache_hit_rate: f64,
    pub cache_savings_usd: f64,
    pub matched_proxy_requests: u64,
    pub bypassed_local_requests: u64,
    pub trend: Vec<ActivityTrendPoint>,
    /// Width of the UTC-aligned trend and cache buckets.
    #[serde(default)]
    pub bucket_seconds: Option<u64>,
    pub heatmap: Vec<HeatmapCell>,
    /// Local weekdays intersecting the requested range, 0 (Mon) .. 6 (Sun).
    #[serde(default)]
    pub covered_weekdays: Vec<u8>,
    pub rhythm: Vec<RhythmBucket>,
    pub by_model: Vec<BreakdownRow>,
    pub by_account: Vec<BreakdownRow>,
    pub by_client: Vec<BreakdownRow>,
    pub by_model_effort: Vec<BreakdownRow>,
    pub board_tiles: Vec<BoardTile>,
    pub detail_retention_days: u32,
}

/// Spillway Compare table — doubles as the in-app list-price table and baseline comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareRow {
    pub key: String,
    pub model: String,
    pub effort: Option<String>,
    pub price: Option<ModelPriceEntry>,
    pub api_equivalent_cost_usd: f64,
    pub requests: u64,
    pub failures: u64,
    pub mean_cost_per_request: Option<f64>,
    pub cost_per_m_tokens: Option<f64>,
    pub cache_hit_pct: f64,
    pub failed_cost_share_pct: f64,
    pub thinking_output_share_pct: f64,
    pub fast_flex_share_pct: f64,
    pub root_tasks: u64,
    pub median_task_cost: Option<f64>,
    pub mean_task_cost: Option<f64>,
    pub p90_task_cost: Option<f64>,
    pub median_tokens_per_task: Option<u64>,
    pub median_requests_per_task: Option<u64>,
    pub mixed_task_share_pct: f64,
    pub other_model_share_pct: f64,
    pub subagent_cost_usd: f64,
    /// Multiples or percentage-point deltas relative to the selected baseline row.
    pub baseline_cost_multiple: Option<f64>,
    pub baseline_req_cost_multiple: Option<f64>,
    pub baseline_cache_pp_delta: Option<f64>,
    pub detail_coverage_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareReport {
    pub baseline_key: Option<String>,
    pub price_as_of: String,
    pub rows: Vec<CompareRow>,
    pub price_catalog: Vec<ModelPriceEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestRow {
    pub id: Uuid,
    pub source_exchange_id: String,
    pub source: ActivitySource,
    pub purpose: ActivityPurpose,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub client: String,
    pub transport: Option<String>,
    pub session_id: Option<String>,
    pub requested_model: String,
    pub effective_model: String,
    pub effort: Option<String>,
    pub outcome: String,
    pub status_code: Option<i32>,
    pub error_message: Option<String>,
    pub latency_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub thinking_tokens: i64,
    pub api_equivalent_cost_usd: Option<f64>,
    pub moved_from_account_id: Option<String>,
    pub attempt_count: u32,
    pub capture_state: CaptureState,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestPage {
    pub items: Vec<RequestRow>,
    pub next_cursor: Option<String>,
    pub total_matched: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptDetail {
    pub id: Uuid,
    pub attempt_index: i32,
    pub provider_id: String,
    pub account_id: Option<String>,
    pub moved_from_account_id: Option<String>,
    pub requested_model: String,
    pub effective_model: String,
    pub effort: Option<String>,
    pub status_code: Option<i32>,
    pub outcome: String,
    pub error_message: Option<String>,
    pub latency_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub thinking_tokens: i64,
    pub api_equivalent_cost_usd: Option<f64>,
    pub capture_state: CaptureState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestDetail {
    pub exchange: RequestRow,
    pub attempts: Vec<AttemptDetail>,
    pub capture_available: bool,
    pub capture_gap_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapturedBodyView {
    pub exchange_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub representation: CaptureState,
    pub redacted: bool,
    pub size_bytes: u64,
    pub payload: Value,
}

// ── Session Traces ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTraceSummary {
    pub session_id: String,
    pub root_session_id: String,
    pub client: String,
    pub provider_id: String,
    pub primary_model: String,
    pub effort: Option<String>,
    pub node_count: u64,
    pub exchange_count: u64,
    pub failure_count: u64,
    pub own_cost_usd: f64,
    pub subtotal_cost_usd: f64,
    pub own_tokens: u64,
    pub subtotal_tokens: u64,
    pub has_unobserved_parent: bool,
    pub has_parent_conflict: bool,
    pub has_cycle: bool,
    pub started_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEdge {
    pub child_session_id: String,
    pub parent_session_id: String,
    pub kind: TraceEdgeKind,
    pub observed_parent: bool,
    pub conflict: bool,
    pub cycle: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceNode {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub edge_kind: Option<TraceEdgeKind>,
    pub depth: u32,
    pub own_requests: u64,
    pub own_failures: u64,
    pub own_tokens: u64,
    pub own_cost_usd: f64,
    pub known_subtotal_cost_usd: f64,
    pub unpriced_requests: u64,
    pub accounts_used: Vec<String>,
    pub moves: Vec<(String, String)>,
    pub exchanges: Vec<RequestRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceDetail {
    pub summary: SessionTraceSummary,
    pub nodes: Vec<TraceNode>,
    pub edges: Vec<TraceEdge>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TracePage {
    pub items: Vec<SessionTraceSummary>,
    pub unassigned_exchanges: u64,
    pub next_cursor: Option<String>,
}

// ── Drift & Canaries ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftDayPoint {
    pub date: String,
    pub replies: u64,
    pub thinking_per_reply: f64,
    pub output_per_reply: f64,
    pub no_thinking_share: f64,
    pub median_ttft_ms: Option<f64>,
    pub failure_rate: f64,
    pub baseline_median_thinking: Option<f64>,
    pub baseline_low: Option<f64>,
    pub baseline_high: Option<f64>,
    /// True only when >= 30 replies/day and >= 5 valid prior days exist.
    pub judgeable: bool,
    pub outlier: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftMarker {
    pub date: String,
    pub kind: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftModelSeries {
    pub model: String,
    pub effort: Option<String>,
    pub days: Vec<DriftDayPoint>,
    pub outlier_days: u64,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CanarySchedule {
    pub enabled: bool,
    /// 6, 12, or 24 hours.
    pub interval_hours: u32,
    pub targets: Vec<CanaryTarget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryTarget {
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryRun {
    pub id: Uuid,
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
    pub prompts_run: u32,
    pub prompts_passed: u32,
    pub mean_thinking_tokens: f64,
    pub mean_ttft_ms: Option<f64>,
    pub ran_at: DateTime<Utc>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftReport {
    pub series: Vec<DriftModelSeries>,
    pub markers: Vec<DriftMarker>,
    pub schedule: CanarySchedule,
    pub recent_canaries: Vec<CanaryRun>,
}

// ── Totals ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TotalsGranularity {
    Hour,
    #[default]
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TotalsBucket {
    pub period: String,
    pub requests: u64,
    pub failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub cache_hit_rate: f64,
    pub thinking_tokens: u64,
    pub api_equivalent_cost_usd: f64,
    pub unpriced_requests: u64,
    pub includes_retained_aggregates: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TotalsReport {
    pub granularity: TotalsGranularity,
    pub buckets: Vec<TotalsBucket>,
    pub grand_total: TotalsBucket,
}

// ── Thinking Analysis & Tests ───────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBucket {
    pub label: String,
    pub turns: u64,
    pub thinking_tokens: u64,
    pub output_tokens: u64,
    pub estimated_thinking_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingAlternative {
    pub label: String,
    pub target_model: String,
    pub target_effort: Option<String>,
    pub estimated_cost_usd: f64,
    pub estimated_savings_usd: f64,
    pub estimated_savings_pct: f64,
    pub is_estimate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingTestRun {
    pub id: Uuid,
    pub baseline_model: String,
    pub baseline_effort: String,
    pub compare_model: String,
    pub compare_effort: Option<String>,
    pub sampled_turns: u32,
    pub judge_model: Option<String>,
    pub equivalence_rate: Option<f64>,
    pub measured_cost_delta_pct: Option<f64>,
    pub ran_at: DateTime<Utc>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingReport {
    pub selected_model: Option<String>,
    pub selected_effort: Option<String>,
    pub verdict: String,
    pub total_turns: u64,
    pub thinking_tokens: u64,
    pub thinking_cost_usd: f64,
    pub buckets: Vec<ThinkingBucket>,
    pub kinds: Vec<ThinkingBucket>,
    pub alternatives: Vec<ThinkingAlternative>,
    pub method_note: String,
    pub test_runs: Vec<ThinkingTestRun>,
    pub board_tiles: Vec<BoardTile>,
}

// ── Evals ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalPredicateKind {
    Contains,
    NotContains,
    Equals,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalPredicate {
    pub kind: EvalPredicateKind,
    pub value: String,
    #[serde(default)]
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCase {
    pub id: Uuid,
    pub name: String,
    pub source_exchange_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
    pub prompt_text: String,
    pub expected_outcome: String,
    pub predicates: Vec<EvalPredicate>,
    pub last_replay: Option<EvalReplayResult>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCaseIn {
    pub name: String,
    #[serde(default)]
    pub source_exchange_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub effort: Option<String>,
    pub prompt_text: String,
    #[serde(default = "default_ok_outcome")]
    pub expected_outcome: String,
    #[serde(default)]
    pub predicates: Vec<EvalPredicate>,
}

fn default_ok_outcome() -> String {
    "ok".to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalPredicateResult {
    pub predicate: EvalPredicate,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReplayResult {
    pub id: Uuid,
    pub case_id: Uuid,
    pub model_id: String,
    pub effort: Option<String>,
    pub account_id: Option<String>,
    pub actual_outcome: String,
    pub response_preview: String,
    pub passed: bool,
    pub predicate_results: Vec<EvalPredicateResult>,
    pub latency_ms: Option<i64>,
    pub replayed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalExportBundle {
    pub schema_version: u32,
    pub exported_at: DateTime<Utc>,
    pub include_bodies: bool,
    pub cases: Vec<EvalCase>,
}

// ── Capture Policy, Diagnostics, Portable Archive, ClickHouse & Settings ────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureHealth {
    pub stored_bodies: u64,
    pub stored_bytes: u64,
    pub gaps: u64,
    pub dropped: u64,
    pub rejected: u64,
    pub over_budget_evictions: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapturePolicy {
    /// Body capture is strictly opt-in (default false).
    pub capture_bodies_enabled: bool,
    /// Detail retention in days (default 35, max 365).
    pub detail_retention_days: u32,
    /// Body retention in days (default 7, max 90).
    pub body_retention_days: u32,
    /// Body storage budget in MiB (default 100, max 1024).
    pub body_budget_mib: u32,
    pub health: CaptureHealth,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            capture_bodies_enabled: false,
            detail_retention_days: 35,
            body_retention_days: 7,
            body_budget_mib: 100,
            health: CaptureHealth { stored_bodies: 0, stored_bytes: 0, gaps: 0, dropped: 0, rejected: 0, over_budget_evictions: 0 },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticBundlePreviewIn {
    pub exchange_ids: Vec<Uuid>,
    #[serde(default)]
    pub include_error_messages: bool,
    #[serde(default)]
    pub include_transformed_bodies: bool,
    #[serde(default)]
    pub include_proxy_logs: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticBundlePreview {
    pub preview_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub exchange_count: usize,
    pub redaction_warnings: Vec<String>,
    pub estimated_bytes: usize,
    pub snapshot: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortableArchiveRequest {
    pub session_id: String,
    #[serde(default)]
    pub include_raw_wire: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortableArchiveExport {
    pub manifest: Value,
    pub warnings: Vec<String>,
    /// Unencrypted JSON Lines payload with credential sanitization applied.
    pub jsonl: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ClickHouseConfig {
    pub enabled: bool,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub source_label: Option<String>,
    #[serde(default)]
    pub export_content: bool,
    #[serde(default)]
    pub export_raw_wire: bool,
    #[serde(default)]
    pub read_through_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ClickHouseStatus {
    pub config: ClickHouseConfig,
    pub connected: bool,
    pub schema_version: Option<u32>,
    pub queued_outbox_rows: u64,
    pub last_delivery_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// Independent numeric tray indicator slot matching Spillway's tray icon configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrayMetricSlot {
    pub id: String,
    pub provider_id: String,
    /// `None` measures the provider pool; `Some` measures a single account.
    pub account_id: Option<String>,
    pub window_key: String,
    pub show_remaining: bool,
    pub warn_threshold_pct: f64,
    pub critical_threshold_pct: f64,
    pub shape: String,
    pub accent_color: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccountSortOrder {
    #[default]
    SoonestReset,
    MostLeft,
    Name,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderPanelSettings {
    pub refresh_interval_secs: u32,
    pub mask_emails: bool,
    pub forecast_horizon_hours: u32,
    pub drain_lookback_hours: u32,
    pub account_sort: AccountSortOrder,
    pub notify_limit_hit: bool,
    pub notify_limit_reset: bool,
    pub notify_account_switch: bool,
    pub notify_proxy_crash: bool,
    #[serde(default)]
    pub solar_zip_or_tz: Option<String>,
    #[serde(default)]
    pub tray_slots: Vec<TrayMetricSlot>,
}

impl Default for ProviderPanelSettings {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 30,
            mask_emails: true,
            forecast_horizon_hours: 12,
            drain_lookback_hours: 6,
            account_sort: AccountSortOrder::SoonestReset,
            notify_limit_hit: true,
            notify_limit_reset: true,
            notify_account_switch: true,
            notify_proxy_crash: true,
            solar_zip_or_tz: None,
            tray_slots: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClearTarget {
    QuotaHistory,
    ActivityDetails,
    ActivityAll,
    CapturedBodies,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClearDataIn {
    pub target: ClearTarget,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClearDataOut {
    pub target: ClearTarget,
    pub rows_deleted: u64,
}

#[cfg(test)]
mod account_error_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_camel_case_fields_and_whole_credential_headers() {
        for field in ["refreshToken", "accessToken", "clientSecret", "apiKey", "idToken", "sessionToken"] {
            assert!(!sanitize_provider_error(&format!(r#"{{"{field}":"opaque-value"}}"#)).contains("opaque-value"));
        }
        for header in ["Cookie", "Set-Cookie", "Authorization", "Proxy-Authorization", "x-api-key", "x-goog-api-key"] {
            let clean = sanitize_provider_error(&format!("{header}: \"a=first\"; b=second, c=third\nKeep this"));
            assert_eq!(clean, "[redacted] Keep this");
        }
        let clean = sanitize_provider_error(r#"{"Cookie":"a=\"example\"; b=second","keep":"yes"}"#);
        assert!(!clean.contains("example") && !clean.contains("second") && clean.contains(r#""keep":"yes""#));
    }

    #[test]
    fn account_errors_extend_legacy_json_and_blocking_status_wins() {
        let mut account: ModelAccount = serde_json::from_value(json!({
            "id": "a", "provider_id": "google", "source_account_id": "a", "display_name": "Account"
        }))
        .unwrap();
        assert!(account.last_error.is_none());
        assert!(serde_json::to_value(&account).unwrap().get("last_error").is_none());
        let now = Utc::now();
        let cooldown = Some(now + chrono::TimeDelta::minutes(1));
        let error: AccountError = serde_json::from_value(json!({
            "kind": "auth_expired", "source": "oauth_refresh", "message": "Sign in\nto continue.", "occurred_at": now
        }))
        .unwrap();
        assert_eq!(error.consecutive_count, 0);
        assert_eq!(error.kind, AccountErrorKind::from_str_lossy(error.kind.as_str()));
        assert_eq!(error.summary(), "Authentication expired: Sign in to continue.");
        assert_eq!(AccountErrorKind::from_str_lossy("future_kind"), AccountErrorKind::Other);
        assert_eq!(AccountStatus::resolve_with_error(true, cooldown, now, true, true, Some(&error)), AccountStatus::Error);
        assert_eq!(AccountStatus::resolve_with_error(true, cooldown, now, true, true, None), AccountStatus::Limited);
        assert_eq!(AccountStatus::resolve_with_error(true, None, now, true, true, None), AccountStatus::Error);
        assert_eq!(AccountStatus::resolve(true, None, now, true, true), AccountStatus::NextPick);
        account.last_error = Some(error.clone());
        let row = serde_json::to_value(&account).unwrap();
        assert!(row["last_error"].get("http_status").is_none());
        assert_eq!(serde_json::from_value::<ModelAccount>(row).unwrap().last_error, Some(error));
    }
}
