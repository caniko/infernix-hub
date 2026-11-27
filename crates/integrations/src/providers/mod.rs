//! Model-provider quota probes and capability-gated control adapters.

pub mod antigravity;
pub mod claude;
pub mod fixture;
pub mod opencode;
pub mod push;
pub mod spillway_proxy;

pub use antigravity::AntigravityProvider;
pub use claude::ClaudeProvider;
pub use fixture::{FullFixtureModelProvider, ReadOnlyFixtureModelProvider};
pub use opencode::OpenCodeProvider;
pub use push::PushModelProvider;
pub use spillway_proxy::SpillwayProxyProvider;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use contracts::{AdapterMode, Verification, providers::*};
use domain::ports::{PortError, PortResult, ProviderIntent};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// An operator-configured credential owned outside the Hub's data store.
#[derive(Clone)]
pub struct AccountTokenFile {
    pub id: String,
    pub label: String,
    pub path: PathBuf,
}

impl AccountTokenFile {
    pub fn new(id: &str, label: &str, path: impl Into<PathBuf>) -> Self {
        Self { id: id.into(), label: label.into(), path: path.into() }
    }
}

pub(super) fn descriptor(id: &str, label: &str, family: &str, mode: AdapterMode) -> ProviderDescriptor {
    ProviderDescriptor {
        id: id.into(),
        label: label.into(),
        family: family.into(),
        mode,
        verification: if mode == AdapterMode::Fixture { Verification::FixtureTested } else { Verification::Built },
        evidence_kinds: [EvidenceKind::AccountInventory, EvidenceKind::QuotaWindows].into(),
        capabilities: BTreeSet::new(),
        router_id: None,
        health: ProviderHealth::Unobserved,
        status_message: None,
        last_observed_at: None,
    }
}

pub(super) fn account(provider: &ProviderDescriptor, id: &str, label: &str) -> ModelAccount {
    ModelAccount {
        id: format!("{}:{id}", provider.id),
        provider_id: provider.id.clone(),
        source_account_id: id.into(),
        display_name: label.into(),
        email: None,
        masked_email: None,
        note: None,
        plan: None,
        tier_multiplier: None,
        observed_capacity_ratio: None,
        status: AccountStatus::Ready,
        enabled: true,
        priority: 0,
        weight: 1,
        cooldown_until: None,
        status_message: None,
        last_error: None,
        last_error_observed: false,
        is_active: false,
        is_preferred: false,
        is_next_pick: false,
        enrollment_state: EnrollmentState::Unobserved,
        host: None,
        unix_user: None,
        credential_fingerprint: None,
        last_used_at: None,
        last_observed_at: None,
        capabilities: provider.capabilities.clone(),
        success_count: 0,
        failure_count: 0,
        windows: Vec::new(),
        reset_grant: None,
        rolling_starter: None,
    }
}

pub(super) fn percent_window(key: &str, label: &str, used: Option<f64>, at: DateTime<Utc>) -> QuotaWindow {
    let used = used.map(|value| value.max(0.0));
    QuotaWindow {
        window_key: key.into(),
        label: label.into(),
        kind: QuotaWindowKind::Custom,
        unit: QuotaUnit::Percent,
        used_percent: used,
        remaining_percent: used.map(|p| (100.0 - p).max(0.0)),
        used_amount: None,
        limit_amount: None,
        currency: None,
        resets_at: None,
        window_duration_mins: None,
        cadence: ResetCadence::Unknown,
        scope_model: None,
        status: QuotaStatus::Unknown,
        observation_state: if used.is_some() { QuotaObservationState::Measured } else { QuotaObservationState::Unavailable },
        observed_at: Some(at),
        stale: false,
        error_reason: None,
    }
}

pub(super) fn record_probe(account: &mut ModelAccount, result: Result<Vec<QuotaWindow>, String>, now: DateTime<Utc>) {
    match result {
        Ok(windows) => {
            account.windows = windows;
            account.last_observed_at = Some(now);
            account.enrollment_state = EnrollmentState::Verified;
        }
        Err(reason) => {
            account.status = AccountStatus::Error;
            account.enrollment_state = EnrollmentState::Error;
            account.status_message = Some(reason.clone());
            let mut window = percent_window("quota", "Quota", None, now);
            window.observation_state = QuotaObservationState::ProbeFailed;
            window.error_reason = Some(reason);
            account.windows.push(window);
        }
    }
}

pub(super) fn batch(provider: &ProviderDescriptor, source: &str, accounts: Vec<ModelAccount>, at: DateTime<Utc>) -> ObservationBatch {
    let health = if accounts.is_empty() {
        ProviderHealth::Unobserved
    } else if accounts.iter().all(|a| a.status == AccountStatus::Error) {
        ProviderHealth::Offline
    } else if accounts
        .iter()
        .any(|a| a.enabled && a.last_error.as_ref().map_or(a.status_message.is_some(), AccountError::is_notification_worthy))
    {
        ProviderHealth::Degraded
    } else {
        ProviderHealth::Healthy
    };
    let reset_grants = accounts.iter().filter_map(|a| Some((a.id.clone(), a.reset_grant.clone()?))).collect();
    ObservationBatch {
        provider_id: provider.id.clone(),
        provider_label: provider.label.clone(),
        provider_family: provider.family.clone(),
        source_host: source.into(),
        source_user: "hub".into(),
        source_publisher: None,
        collected_at: at,
        account_inventory_complete: false,
        health,
        status_message: None,
        router_id: provider.router_id.clone(),
        capabilities: provider.capabilities.clone(),
        evidence_kinds: provider.evidence_kinds.clone(),
        accounts,
        quota_samples: Vec::new(),
        reset_grants,
        key_groups: Vec::new(),
        models: Vec::new(),
        routing_status: None,
        proxy_logs: Vec::new(),
    }
}

pub(super) fn str_field(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

pub(super) fn rows(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

pub(super) fn iso(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str()?.parse().ok()
}

pub(super) fn seconds_or_iso(value: &Value) -> Option<DateTime<Utc>> {
    iso(value).or_else(|| DateTime::from_timestamp(value.as_i64()?, 0))
}

pub(super) fn uint(value: &Value) -> Option<u32> {
    value.as_u64()?.try_into().ok()
}

pub(super) fn fingerprint(namespace: &str, value: &str) -> String {
    hex::encode(Sha256::digest(format!("{namespace}\0{value}")))
}

pub(super) fn reset_grants(raw: &Value, observed_at: DateTime<Utc>) -> Vec<ResetGrant> {
    let checked_at = seconds_or_iso(&raw["checked_at"]).unwrap_or(observed_at);
    let convert = |g: &Value, id: String, expires_at: Option<DateTime<Utc>>, malformed_expiry| {
        let available_count = uint(&g["available"]).or_else(|| uint(&g["available_count"]))?;
        let applicable = g["applicable"].as_bool().unwrap_or_else(|| uint(&g["applicable"]).unwrap_or_default() > 0);
        let unavailable_reason = if malformed_expiry {
            Some("invalid reset grant expiry".into())
        } else if !g["unavailable_reason"].is_null() {
            Some("upstream marked the reset grant unavailable".into())
        } else {
            None
        };
        Some(ResetGrant {
            grant_id: id,
            available_count,
            applicable: applicable
                && available_count > 0
                && unavailable_reason.is_none()
                && g["scope"].as_str() != Some("unknown")
                && expires_at.is_none_or(|at| at > observed_at),
            checked_at,
            expires_at,
            scope: str_field(&g["scope"]).map(str::to_owned),
            cleared_windows: rows(g.get("clears").unwrap_or(&g["cleared_windows"]))
                .iter()
                .filter_map(str_field)
                .map(str::to_owned)
                .collect(),
            weekly_credit_back: g["gives_back_weekly"].as_bool().or_else(|| g["weekly_credit_back"].as_bool()).unwrap_or(false),
            unavailable_reason,
        })
    };
    if raw.get("grants").is_some() {
        rows(&raw["grants"])
            .iter()
            .filter_map(|g| {
                let id = str_field(&g["grant_id"]).or_else(|| str_field(&g["id"]))?;
                let expiry = seconds_or_iso(&g["expires_at"]);
                convert(g, id.into(), expiry, !g["expires_at"].is_null() && expiry.is_none())
            })
            .collect()
    } else {
        let expiry = seconds_or_iso(&raw["expires_at"]).or_else(|| rows(&raw["expiries"]).iter().filter_map(seconds_or_iso).min());
        convert(
            raw,
            str_field(&raw["grant_id"]).unwrap_or("reset_credits").into(),
            expiry,
            !raw["expires_at"].is_null() && expiry.is_none(),
        )
        .into_iter()
        .collect()
    }
}

/// Keep status and a local operation label, never URL, credentials, or remote text.
fn safe_error(operation: &'static str, status: Option<u16>, reason: &'static str) -> String {
    match status {
        Some(code) => format!("{operation}: HTTP {code}"),
        None => format!("{operation}: {reason}"),
    }
}

/// Build the no-redirect HTTP client to pass to the authenticated probes.
pub fn http_client() -> PortResult<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| PortError::Unreachable("could not create provider HTTP client".into()))
}

/// Operator-configured URLs may target private services; only loopback may use plaintext.
pub(super) fn base_url(raw: &str) -> PortResult<Url> {
    let bad = || PortError::Rejected {
        status: 400,
        message: "invalid provider endpoint (use HTTPS or loopback HTTP; no credentials, query or fragment)".into(),
    };
    let url = Url::parse(raw.trim_end_matches('/')).map_err(|_| bad())?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost" || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    if url.host_str().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(bad());
    }
    Ok(url)
}

pub(super) fn endpoint(base: &Url, suffix: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{}", base.path().trim_end_matches('/'), suffix));
    url
}

pub(super) async fn secret_file(path: &Path) -> Result<String, String> {
    let secret = tokio::fs::read_to_string(path).await.map_err(|_| "credential file is unreadable".to_string())?;
    let secret = secret.trim();
    if secret.is_empty() { Err("credential file is empty".into()) } else { Ok(secret.into()) }
}

pub(super) async fn request(builder: RequestBuilder, operation: &'static str, mutates: bool, timeout: Duration) -> PortResult<Response> {
    let response = builder.timeout(timeout).send().await.map_err(|error| {
        let reason = if error.is_timeout() { "request timed out" } else { "request failed" };
        let message = safe_error(operation, None, reason);
        if mutates && !error.is_connect() && !error.is_builder() { PortError::Ambiguous(message) } else { PortError::Unreachable(message) }
    })?;
    let status = response.status();
    if !status.is_success() {
        let message = safe_error(operation, Some(status.as_u16()), "");
        return Err(if mutates && status.is_server_error() {
            PortError::Ambiguous(message)
        } else {
            PortError::Rejected { status: status.as_u16(), message }
        });
    }
    Ok(response)
}

pub(super) async fn json_body(mut response: Response, operation: &'static str) -> PortResult<Value> {
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    let invalid = || PortError::Unreachable(safe_error(operation, None, "invalid or oversized response"));
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| invalid())? {
        if chunk.len() > MAX_BYTES - body.len() {
            return Err(invalid());
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| invalid())
}

pub(super) async fn probe_json(builder: RequestBuilder, operation: &'static str) -> Result<Value, String> {
    let response = request(builder, operation, false, Duration::from_secs(10)).await.map_err(|e| e.to_string())?;
    json_body(response, operation).await.map_err(|e| e.to_string())
}

// Each typed facet advertises only its own subset, and the same check guards direct submission.
const QUOTA: &[ProviderCapability] = &[
    ProviderCapability::RefreshQuota,
    ProviderCapability::RefreshGrants,
    ProviderCapability::ClaimResetGrant,
    ProviderCapability::StartRollingWindow,
];
const ACCOUNTS: &[ProviderCapability] = &[
    ProviderCapability::SelectLocalAccount,
    ProviderCapability::SelectPersistentAccount,
    ProviderCapability::SetEnabled,
    ProviderCapability::SetPriority,
    ProviderCapability::SetWeight,
    ProviderCapability::SetCapacity,
    ProviderCapability::SetNote,
];
const CREDENTIALS: &[ProviderCapability] = &[
    ProviderCapability::BeginOAuth,
    ProviderCapability::RenewLogin,
    ProviderCapability::RemoveLogin,
    ProviderCapability::ImportLogin,
    ProviderCapability::ExportLogin,
    ProviderCapability::RevokeGrant,
];
const KEY_GROUPS: &[ProviderCapability] = &[
    ProviderCapability::ManageKeyGroups,
    ProviderCapability::ManageKeys,
    ProviderCapability::ToggleKey,
    ProviderCapability::SetGroupEnabled,
    ProviderCapability::SetRequestIdentity,
];
const MODELS: &[ProviderCapability] =
    &[ProviderCapability::ConfigureModels, ProviderCapability::DiscoverModels, ProviderCapability::SetModelAliases];
const ROUTING: &[ProviderCapability] = &[
    ProviderCapability::ReadProxyStatus,
    ProviderCapability::ControlProxy,
    ProviderCapability::ConfigureProxy,
    ProviderCapability::SetRoutePolicy,
    ProviderCapability::SetPoolOverride,
    ProviderCapability::MoveConversation,
    ProviderCapability::ReadClientWiring,
    ProviderCapability::ApplyClientWiring,
    ProviderCapability::UndoClientWiring,
    ProviderCapability::ManageClientKey,
    ProviderCapability::ManageLockouts,
    ProviderCapability::ReadProxyLogs,
];
const PROBES: &[ProviderCapability] =
    &[ProviderCapability::RunCanary, ProviderCapability::RunThinkingTest, ProviderCapability::ReplayTextEval];
const FACETS: &[&[ProviderCapability]] = &[QUOTA, ACCOUNTS, CREDENTIALS, KEY_GROUPS, MODELS, ROUTING, PROBES];

pub(super) fn check_target(provider: &ProviderDescriptor, intent: &ProviderIntent) -> PortResult<()> {
    if intent.provider_id == provider.id { Ok(()) } else { Err(PortError::NotFound) }
}

macro_rules! typed_facets {
    ($provider:ty: $($field:ident => $command:ident, $variant:ident, $capabilities:ident);+ $(;)?) => {
        impl $provider {
            fn command_facets(&self) -> domain::ports::ProviderFacets<'_> {
                let mut facets = domain::ports::ProviderFacets::observation_only(self.descriptor.evidence_kinds.clone());
                $(if $capabilities.iter().any(|cap| self.descriptor.capabilities.contains(cap)) {
                    facets.$field = Some(self);
                })+
                facets
            }
        }
        $(
            #[async_trait::async_trait]
            impl domain::ports::ProviderOps<$command> for $provider {
                fn supported(&self) -> ProviderCapabilities {
                    $capabilities.iter().copied().filter(|cap| self.descriptor.capabilities.contains(cap)).collect()
                }

                async fn submit(&self, intent: &ProviderIntent, command: &$command) -> PortResult<domain::ports::ProviderDispatch> {
                    check_target(&self.descriptor, intent)?;
                    let command = ProviderCommand::$variant(command.clone());
                    if !self.descriptor.capabilities.contains(&command.required_capability()) {
                        return Err(PortError::Unsupported(command.kind_name().into()));
                    }
                    self.submit_command(intent, command).await
                }

                async fn reconcile(&self, intent: &ProviderIntent) -> PortResult<domain::ports::ProviderReconciliation> {
                    check_target(&self.descriptor, intent)?;
                    self.reconcile_intent(intent).await
                }
            }
        )+
    };
}

use typed_facets;
