use super::*;
use async_trait::async_trait;
use chrono::TimeDelta;
use domain::ports::{ModelProvider, ProviderDispatch, ProviderFacets, ProviderReconciliation};
use serde_json::json;
use std::collections::BTreeMap;
use tokio::sync::RwLock;
use uuid::Uuid;

struct State {
    snapshot: ObservationBatch,
    receipts: BTreeMap<Uuid, (String, ProviderDispatch)>,
    schedule: CanarySchedule,
    sessions: BTreeMap<String, String>,
}

pub struct FullFixtureModelProvider {
    descriptor: ProviderDescriptor,
    started_at: DateTime<Utc>,
    state: RwLock<State>,
}

impl FullFixtureModelProvider {
    pub fn new(id: &str, label: &str, family: &str) -> Self {
        let at = Utc::now();
        let mut descriptor = descriptor(id, label, family, AdapterMode::Fixture);
        descriptor.capabilities.extend(FACETS.iter().flat_map(|caps| caps.iter().copied()));
        descriptor.evidence_kinds.extend([EvidenceKind::ResetGrants, EvidenceKind::RequestMetadata, EvidenceKind::TraceEdges]);
        let router = format!("{id}-fixture");
        descriptor.router_id = Some(router.clone());
        descriptor.health = ProviderHealth::Healthy;
        let mut accounts = Vec::new();
        for (i, name) in ["Primary subscription", "Secondary subscription", "Overflow subscription"].into_iter().enumerate() {
            let mut account = account(&descriptor, &format!("subscription-{}", i + 1), name);
            account.plan = Some(if i == 0 { "Max 5×" } else { "Pro" }.into());
            account.tier_multiplier = Some(if i == 0 { 5.0 } else { 1.0 });
            account.is_active = i == 0;
            account.is_next_pick = i == 1;
            account.status = AccountStatus::resolve(true, None, at, account.is_next_pick, false);
            account.enrollment_state = if i == 0 { EnrollmentState::Selected } else { EnrollmentState::Verified };
            account.host = Some("fixture-host".into());
            account.unix_user = Some("fixture-user".into());
            account.last_observed_at = Some(at);
            account.last_used_at = Some(at - TimeDelta::minutes(10 * (i as i64 + 1)));
            account.success_count = 120 + i as u64 * 38;
            account.failure_count = i as u64 + 2;
            account.windows = [
                ("five_hour", "5-hour", QuotaWindowKind::Session, 300, [82.0, 36.0, 0.0][i]),
                ("seven_day", "Weekly", QuotaWindowKind::Weekly, 10080, [64.0, 26.0, 9.0][i]),
            ]
            .into_iter()
            .map(|(key, label, kind, minutes, used)| {
                let mut w = percent_window(key, label, Some(used), at);
                w.kind = kind;
                w.window_duration_mins = Some(minutes);
                w.resets_at = (used > 0.0).then_some(at + TimeDelta::minutes(i64::from(minutes / (i as u32 + 3))));
                w.cadence = ResetCadence::RollingOnUse;
                w.status = if used >= 80.0 { QuotaStatus::Warning } else { QuotaStatus::Allowed };
                w
            })
            .collect();
            if i == 0 {
                account.reset_grant = Some(ResetGrant {
                    grant_id: "fixture-reset".into(),
                    available_count: 2,
                    applicable: true,
                    checked_at: at,
                    expires_at: Some(at + TimeDelta::days(10)),
                    scope: Some("general".into()),
                    cleared_windows: vec!["five_hour".into()],
                    weekly_credit_back: true,
                    unavailable_reason: None,
                });
            }
            account.rolling_starter = Some(RollingWindowStatus::default());
            accounts.push(account);
        }
        let mut snapshot = batch(&descriptor, "fixture-host", accounts, at);
        snapshot.source_user = "fixture-user".into();
        let group_id = format!("{id}:fixture-keys");
        let models = sample_models(family);
        let models: Vec<_> = models.iter().map(|model| sample_model(&descriptor, model, Some(&group_id), at)).collect();
        snapshot.models = models.clone();
        snapshot.key_groups.push(KeyGroup {
            id: group_id,
            provider_id: id.into(),
            family: family.into(),
            name: "Fixture API keys".into(),
            base_url: None,
            enabled: true,
            request_identity: RequestIdentityMode::Api,
            keys: vec![MaskedApiKey {
                id: format!("{id}:fixture-key"),
                label: "Example key".into(),
                key_hint: "••••demo".into(),
                enabled: true,
                created_at: at,
                last_used_at: None,
                success_count: 0,
                failure_count: 0,
            }],
            configured_models: models.iter().map(|m| m.id.clone()).collect(),
            discovered_models: models,
            aliases: Vec::new(),
            updated_at: at,
        });
        snapshot.proxy_logs.push(log(at, "Fixture proxy started"));
        snapshot.routing_status = Some(RoutingView {
            provider_id: id.into(),
            router_id: Some(router.clone()),
            revision: 0,
            policy: RoutingPolicy::default(),
            runtime: Some(ProxyRuntimeStatus {
                router_id: router,
                running: true,
                endpoint: Some("http://127.0.0.1:8317".into()),
                port: Some(8317),
                lan_bind: false,
                auto_start: true,
                debug_logs: false,
                build_version: Some("fixture-1".into()),
                client_key_hint: Some("••••demo".into()),
                shared_profile_owner: None,
                crash_restarts: 0,
                last_error: None,
                updated_at: at,
            }),
            clients: vec![ClientWiringStatus {
                client_id: "opencode".into(),
                display_name: "OpenCode".into(),
                state: ClientWiringState::NotApplied,
                backup_available: false,
                managed_by: None,
                detail: None,
                instructions: None,
            }],
            lockouts: vec![ProxyLockout {
                id: "fixture-lockout".into(),
                tool_name: "Example outside client".into(),
                reason: "Fixture client key mismatch".into(),
                locked_at: at,
            }],
            recent_logs: snapshot.proxy_logs.clone(),
            rolling_starter: RollingWindowStarterConfig::default(),
        });
        Self {
            descriptor,
            started_at: at,
            state: RwLock::new(State {
                snapshot,
                receipts: BTreeMap::new(),
                schedule: CanarySchedule::default(),
                sessions: BTreeMap::new(),
            }),
        }
    }

    async fn submit_command(&self, intent: &ProviderIntent, command: ProviderCommand) -> PortResult<ProviderDispatch> {
        let mut state = self.state.write().await;
        let digest = fingerprint("fixture-operation", &json!([intent, command]).to_string());
        if let Some((previous, receipt)) = state.receipts.get(&intent.operation_id) {
            return if *previous == digest { Ok(receipt.clone()) } else { Err(reject(409, "fixture operation ID already used")) };
        }
        let at = Utc::now();
        let receipt = match command {
            ProviderCommand::Quota(c) => apply_quota(&mut state.snapshot, c, at)?,
            ProviderCommand::Account(c) => apply_account(&mut state.snapshot, c, at)?,
            ProviderCommand::Credential(c) => self.apply_credential(&mut state.snapshot, intent, c, at)?,
            ProviderCommand::KeyGroup(c) => self.apply_group(&mut state.snapshot, c, at)?,
            ProviderCommand::Model(c) => self.apply_model(&mut state.snapshot, c, at)?,
            ProviderCommand::Routing(c) => apply_routing(&mut state, intent, c, at)?,
            ProviderCommand::Probe(c) => apply_probe(&mut state, intent, c, at)?,
        };
        state.receipts.insert(intent.operation_id, (digest, receipt.clone()));
        Ok(receipt)
    }

    async fn reconcile_intent(&self, intent: &ProviderIntent) -> PortResult<ProviderReconciliation> {
        Ok(match self.state.read().await.receipts.get(&intent.operation_id) {
            Some((_, ProviderDispatch::Completed { receipt })) => ProviderReconciliation::Applied { receipt: receipt.clone() },
            Some(_) => ProviderReconciliation::StillPending,
            None => ProviderReconciliation::NotApplied { reason: "fixture operation was not applied".into() },
        })
    }

    fn apply_credential(
        &self,
        snapshot: &mut ObservationBatch,
        intent: &ProviderIntent,
        command: CredentialCommand,
        at: DateTime<Utc>,
    ) -> PortResult<ProviderDispatch> {
        match command {
            CredentialCommand::BeginOAuth { .. } => {
                return Ok(ProviderDispatch::AwaitingOperator {
                    message: "Fixture only: no sign-in or credential exchange was performed".into(),
                    receipt: Some(json!({"fixture": true, "session_id": intent.operation_id, "state": "awaiting_operator"})),
                });
            }
            CredentialCommand::RenewLogin { account_id } => {
                for account in selected_accounts(snapshot, account_id.as_deref())? {
                    account.enrollment_state = EnrollmentState::Verified;
                    account.last_observed_at = Some(at);
                }
            }
            CredentialCommand::RemoveLogin { account_id } => {
                let account = find_account(snapshot, &account_id)?;
                account.enabled = false;
                account.is_active = false;
                account.is_preferred = false;
                account.is_next_pick = false;
                account.status = AccountStatus::Disabled;
                account.enrollment_state = EnrollmentState::Catalogued;
                account.reset_grant = None;
            }
            CredentialCommand::ImportLogin { label, .. } => {
                let id = intent.operation_id.to_string();
                let mut account = account(&self.descriptor, &id, label.as_deref().unwrap_or("Imported fixture login"));
                account.enrollment_state = EnrollmentState::Deployed;
                let account_id = account.id.clone();
                snapshot.accounts.push(account);
                return complete(json!({"fixture": true, "imported": true, "account_id": account_id}));
            }
            CredentialCommand::ExportLogin { account_id } => {
                find_account(snapshot, &account_id)?;
                return complete(json!({"fixture": true, "exported": false, "reason": "fixture stores no login secrets"}));
            }
            CredentialCommand::RevokeGrant { account_id } => {
                let grant = find_account(snapshot, &account_id)?.reset_grant.as_mut().ok_or(PortError::NotFound)?;
                grant.available_count = 0;
                grant.applicable = false;
                grant.checked_at = at;
                grant.unavailable_reason = Some("Fixture grant revoked".into());
            }
        }
        complete(json!({"fixture": true, "applied": true}))
    }

    fn apply_group(&self, snapshot: &mut ObservationBatch, command: KeyGroupCommand, at: DateTime<Utc>) -> PortResult<ProviderDispatch> {
        let group_id = match command {
            KeyGroupCommand::UpsertGroup { group_id, family, name, base_url, enabled, request_identity, configured_models, aliases } => {
                let base_url = base_url.as_deref().map(super::base_url).transpose()?.map(|url| url.to_string());
                let group_id = group_id.unwrap_or_else(|| format!("{}:{}", self.descriptor.id, Uuid::new_v4()));
                if let Some(group) = snapshot.key_groups.iter_mut().find(|g| g.id == group_id) {
                    group.family = family;
                    group.name = name;
                    group.base_url = base_url;
                    group.enabled = enabled;
                    group.request_identity = request_identity;
                    group.configured_models = configured_models;
                    group.aliases = aliases;
                    group.updated_at = at;
                } else {
                    snapshot.key_groups.push(KeyGroup {
                        id: group_id.clone(),
                        provider_id: self.descriptor.id.clone(),
                        family,
                        name,
                        base_url,
                        enabled,
                        request_identity,
                        configured_models,
                        aliases,
                        keys: Vec::new(),
                        discovered_models: Vec::new(),
                        updated_at: at,
                    });
                }
                group_id
            }
            KeyGroupCommand::DeleteGroup { group_id } => {
                find_group(snapshot, &group_id)?;
                snapshot.key_groups.retain(|g| g.id != group_id);
                snapshot.models.retain(|m| m.group_id.as_ref() != Some(&group_id));
                group_id
            }
            KeyGroupCommand::AddKey { group_id, label, .. } => {
                let group = find_group(snapshot, &group_id)?;
                let id = Uuid::new_v4().to_string();
                group.keys.push(MaskedApiKey {
                    id: id.clone(),
                    label,
                    key_hint: "••••fixture".into(),
                    enabled: true,
                    created_at: at,
                    last_used_at: None,
                    success_count: 0,
                    failure_count: 0,
                });
                group.updated_at = at;
                return complete(json!({"fixture": true, "key_id": id}));
            }
            KeyGroupCommand::ToggleKey { group_id, key_id, enabled } => {
                let group = find_group(snapshot, &group_id)?;
                group.keys.iter_mut().find(|k| k.id == key_id).ok_or(PortError::NotFound)?.enabled = enabled;
                group_id
            }
            KeyGroupCommand::RemoveKey { group_id, key_id } => {
                let group = find_group(snapshot, &group_id)?;
                let index = group.keys.iter().position(|k| k.id == key_id).ok_or(PortError::NotFound)?;
                group.keys.remove(index);
                group_id
            }
            KeyGroupCommand::SetGroupEnabled { group_id, enabled } => {
                find_group(snapshot, &group_id)?.enabled = enabled;
                group_id
            }
            KeyGroupCommand::SetRequestIdentity { group_id, mode } => {
                find_group(snapshot, &group_id)?.request_identity = mode;
                group_id
            }
        };
        if let Ok(group) = find_group(snapshot, &group_id) {
            group.updated_at = at;
        }
        complete(json!({"fixture": true, "applied": true}))
    }

    fn apply_model(&self, snapshot: &mut ObservationBatch, command: ModelCommand, at: DateTime<Utc>) -> PortResult<ProviderDispatch> {
        match command {
            ModelCommand::DiscoverModels { group_id } => {
                if let Some(id) = &group_id {
                    find_group(snapshot, id)?;
                }
                let mut count = 0;
                for group in snapshot.key_groups.iter_mut().filter(|g| group_id.as_ref().is_none_or(|id| id == &g.id)) {
                    let defaults = sample_models(&group.family);
                    let ids: BTreeSet<_> = defaults.into_iter().chain(group.configured_models.iter().map(String::as_str)).collect();
                    group.discovered_models = ids.into_iter().map(|id| sample_model(&self.descriptor, id, Some(&group.id), at)).collect();
                    group.updated_at = at;
                    count += group.discovered_models.len();
                }
                snapshot.models = snapshot.key_groups.iter().flat_map(|g| g.discovered_models.clone()).collect();
                return complete(json!({"fixture": true, "models_discovered": count}));
            }
            ModelCommand::SetModels { group_id, models } => {
                let group = find_group(snapshot, &group_id)?;
                group.configured_models = models;
                group.updated_at = at;
            }
            ModelCommand::SetAliases { group_id, aliases } => {
                let group = find_group(snapshot, &group_id)?;
                group.aliases = aliases;
                group.updated_at = at;
            }
        }
        complete(json!({"fixture": true, "applied": true}))
    }

    /// Stable IDs and timestamps for repeatable imports, with enough daily data for drift baselines.
    pub fn seed_activity(&self) -> ActivityIngestBatch {
        let provider = &self.descriptor.id;
        let [main, other] = sample_models(&self.descriptor.family);
        let mut exchanges = Vec::new();
        let midnight = DateTime::from_naive_utc_and_offset(self.started_at.date_naive().and_hms_opt(0, 0, 0).expect("midnight"), Utc);
        for day in 0..9 {
            for i in 0..38 {
                let at = midnight - TimeDelta::days(9 - day) + TimeDelta::minutes(8 * 60 + i * 14);
                let parent = format!("{provider}:fixture-{day}-{}", i / 4);
                let child = i % 4 == 2;
                let model = if i % 8 == 7 { other } else { main };
                let account_id = format!("{provider}:subscription-{}", 1 + (i + day) % 3);
                let retry = i % 13 == 0;
                let failure = i % 19 == 3;
                let thinking = if failure || i % 9 == 4 {
                    0
                } else if day == 7 {
                    170 + i * 5
                } else {
                    760 + i * 7
                };
                let id = format!("{provider}:fixture-exchange-{day}-{i}");
                let moved_from = retry.then(|| format!("{provider}:subscription-{}", 1 + (i + day + 1) % 3));
                let outcome = if failure { "upstream_error" } else { "ok" };
                let code = if failure { 503 } else { 200 };
                let latency = 820 + i * 19 + if retry { 400 } else { 0 };
                let ttft = 260 + i * 8;
                let input = 1800 + i * 419;
                let output = if failure { 0 } else { 490 + thinking + i * 3 };
                let cache_read = if i % 3 == 0 { 0 } else { input / 2 };
                let fast = i % 7 == 2;
                let attempt = |index, account, moved, code, outcome: &str, elapsed, tokens, started| ActivityAttemptIn {
                    source_attempt_id: format!("{id}:{index}"),
                    attempt_index: index,
                    provider_id: provider.clone(),
                    account_id: Some(account),
                    moved_from_account_id: moved,
                    requested_model: main.into(),
                    effective_model: model.into(),
                    effort: Some("high".into()),
                    status_code: Some(code),
                    outcome: outcome.into(),
                    error_message: match code {
                        429 => Some("Fixture subscription limit".into()),
                        503 => Some("Fixture upstream unavailable".into()),
                        _ => None,
                    },
                    latency_ms: Some(elapsed),
                    ttft_ms: (code == 200).then_some(ttft),
                    input_tokens: if code == 429 { 0 } else { input },
                    output_tokens: if code == 200 { output } else { 0 },
                    cache_read_tokens: if code == 429 { 0 } else { cache_read },
                    cache_write_tokens: 0,
                    thinking_tokens: tokens,
                    fast_mode: fast,
                    capture_state: CaptureState::MetadataOnly,
                    captured_body: None,
                    started_at: started,
                    finished_at: Some(started + TimeDelta::milliseconds(elapsed)),
                };
                let mut attempts = Vec::new();
                if let Some(from) = &moved_from {
                    attempts.push(attempt(0, from.clone(), None, 429, "rate_limited", 400, 0, at));
                }
                attempts.push(attempt(
                    i32::from(retry),
                    account_id.clone(),
                    moved_from,
                    code,
                    outcome,
                    latency - if retry { 400 } else { 0 },
                    thinking,
                    at + TimeDelta::milliseconds(if retry { 400 } else { 0 }),
                ));
                exchanges.push(ActivityExchangeIn {
                    source_exchange_id: id,
                    source: if i % 11 == 5 { ActivitySource::LocalImport } else { ActivitySource::Proxy },
                    purpose: match i {
                        31 => ActivityPurpose::Canary,
                        32 => ActivityPurpose::ThinkingTest,
                        33 => ActivityPurpose::EvalReplay,
                        34 => ActivityPurpose::WindowStarter,
                        _ => ActivityPurpose::Ordinary,
                    },
                    client: if i % 6 == 5 { "claude-code" } else { "opencode" }.into(),
                    client_version: Some(if day < 6 { "1.8.0" } else { "1.9.0" }.into()),
                    transport: Some(if i % 11 == 5 { "local-import" } else { "http" }.into()),
                    session_id: Some(if child { format!("{parent}-subagent") } else { parent.clone() }),
                    parent_session_id: child.then_some(parent),
                    parent_edge_kind: child.then_some(TraceEdgeKind::Subagent),
                    provider_id: provider.clone(),
                    account_id: Some(account_id),
                    requested_model: main.into(),
                    effective_model: model.into(),
                    effort: Some("high".into()),
                    outcome: outcome.into(),
                    status_code: Some(code),
                    error_message: failure.then_some("Fixture upstream unavailable".into()),
                    latency_ms: Some(latency),
                    ttft_ms: (!failure).then_some(ttft),
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_tokens: cache_read,
                    cache_write_tokens: 0,
                    thinking_tokens: thinking,
                    fast_mode: fast,
                    capture_state: CaptureState::MetadataOnly,
                    searchable_text: Some(if child { "Fixture subagent verified tests" } else { "Fixture coding task" }.into()),
                    occurred_at: at,
                    attempts,
                });
            }
        }
        ActivityIngestBatch { source_host: "fixture-host".into(), source_user: "fixture-user".into(), exchanges }
    }
}

typed_facets!(FullFixtureModelProvider:
    quota => QuotaCommand, Quota, QUOTA;
    accounts => AccountCommand, Account, ACCOUNTS;
    credentials => CredentialCommand, Credential, CREDENTIALS;
    key_groups => KeyGroupCommand, KeyGroup, KEY_GROUPS;
    models => ModelCommand, Model, MODELS;
    routing => RoutingCommand, Routing, ROUTING;
    probes => ProbeCommand, Probe, PROBES;
);

#[async_trait]
impl ModelProvider for FullFixtureModelProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }
    fn facets(&self) -> ProviderFacets<'_> {
        self.command_facets()
    }
    async fn collect(&self) -> PortResult<Option<ObservationBatch>> {
        let mut snapshot = self.state.read().await.snapshot.clone();
        snapshot.collected_at = Utc::now();
        snapshot.reset_grants = snapshot.accounts.iter().filter_map(|a| Some((a.id.clone(), a.reset_grant.clone()?))).collect();
        Ok(Some(snapshot))
    }
}

fn sample_models(family: &str) -> [&'static str; 2] {
    match family {
        "antigravity" | "gemini" => ["gemini-2.5-pro", "gemini-2.5-flash"],
        "opencode" | "openai" | "codex" => ["gpt-5", "gpt-5-mini"],
        _ => ["claude-sonnet-4-6", "claude-haiku-4-5"],
    }
}

fn sample_model(provider: &ProviderDescriptor, id: &str, group: Option<&str>, at: DateTime<Utc>) -> DiscoveredModel {
    DiscoveredModel {
        id: id.into(),
        display_name: Some(id.into()),
        family: Some(provider.family.clone()),
        provider_id: provider.id.clone(),
        group_id: group.map(str::to_owned),
        context_window: Some(200_000),
        supports_thinking: Some(true),
        discovered_at: Some(at),
    }
}

fn complete(receipt: Value) -> PortResult<ProviderDispatch> {
    Ok(ProviderDispatch::Completed { receipt })
}

fn reject(status: u16, message: &str) -> PortError {
    PortError::Rejected { status, message: message.into() }
}

fn find_account<'a>(snapshot: &'a mut ObservationBatch, id: &str) -> PortResult<&'a mut ModelAccount> {
    snapshot.accounts.iter_mut().find(|a| a.id == id).ok_or(PortError::NotFound)
}

fn selected_accounts<'a>(snapshot: &'a mut ObservationBatch, id: Option<&str>) -> PortResult<Vec<&'a mut ModelAccount>> {
    if let Some(id) = id { Ok(vec![find_account(snapshot, id)?]) } else { Ok(snapshot.accounts.iter_mut().collect()) }
}

fn find_group<'a>(snapshot: &'a mut ObservationBatch, id: &str) -> PortResult<&'a mut KeyGroup> {
    snapshot.key_groups.iter_mut().find(|g| g.id == id).ok_or(PortError::NotFound)
}

fn log(at: DateTime<Utc>, message: &str) -> ProxyLogEntry {
    ProxyLogEntry { at, stream: "management".into(), level: "info".into(), message: message.into() }
}

fn apply_quota(snapshot: &mut ObservationBatch, command: QuotaCommand, at: DateTime<Utc>) -> PortResult<ProviderDispatch> {
    match command {
        QuotaCommand::RefreshQuota { account_id } => {
            for account in selected_accounts(snapshot, account_id.as_deref())? {
                account.last_observed_at = Some(at);
                for w in &mut account.windows {
                    w.observed_at = Some(at);
                }
            }
        }
        QuotaCommand::RefreshGrants { account_id } => {
            for account in selected_accounts(snapshot, account_id.as_deref())? {
                if let Some(grant) = &mut account.reset_grant {
                    grant.checked_at = at;
                }
            }
        }
        QuotaCommand::ClaimResetGrant { account_id, grant_id } => {
            let account = find_account(snapshot, &account_id)?;
            let grant = account.reset_grant.as_mut().filter(|g| g.grant_id == grant_id).ok_or(PortError::NotFound)?;
            if !grant.applicable || grant.available_count == 0 || grant.expires_at.is_some_and(|expiry| expiry <= at) {
                return Err(reject(409, "fixture grant is not applicable"));
            }
            grant.available_count -= 1;
            grant.applicable = grant.available_count > 0;
            grant.checked_at = at;
            for w in account.windows.iter_mut().filter(|w| grant.cleared_windows.contains(&w.window_key)) {
                w.used_percent = Some(0.0);
                w.remaining_percent = Some(100.0);
                w.observed_at = Some(at);
                w.resets_at = None;
                w.status = QuotaStatus::Allowed;
            }
            account.last_observed_at = Some(at);
            return complete(
                json!({"fixture": true, "reset_applied": true, "remaining_grants": grant.available_count, "windows_cleared": grant.cleared_windows}),
            );
        }
        QuotaCommand::ConfigureRollingStarter { config } => {
            for account in &mut snapshot.accounts {
                account.rolling_starter.get_or_insert_default().enabled = config.enabled;
            }
            let routing = snapshot.routing_status.as_mut().expect("fixture routing");
            routing.rolling_starter = config;
            routing.revision += 1;
        }
        QuotaCommand::TriggerRollingStart { account_id, .. } => {
            let account = find_account(snapshot, &account_id)?;
            let w = account.windows.iter_mut().find(|w| w.kind == QuotaWindowKind::Session).ok_or(PortError::NotFound)?;
            if w.resets_at.is_some_and(|reset| reset > at) {
                return Err(reject(409, "fixture rolling window is already active"));
            }
            w.used_percent = Some(1.0);
            w.remaining_percent = Some(99.0);
            w.observed_at = Some(at);
            w.resets_at = Some(at + TimeDelta::hours(5));
            let status = account.rolling_starter.get_or_insert_default();
            status.last_started_at = Some(at);
            status.active_window_started_at = Some(at);
            account.last_observed_at = Some(at);
        }
    }
    complete(json!({"fixture": true, "observed_at": at}))
}

fn apply_account(snapshot: &mut ObservationBatch, command: AccountCommand, at: DateTime<Utc>) -> PortResult<ProviderDispatch> {
    match command {
        AccountCommand::SelectLocalAccount { account_id, host, unix_user } => {
            if !find_account(snapshot, &account_id)?.enabled {
                return Err(reject(409, "fixture account is disabled"));
            }
            for a in &mut snapshot.accounts {
                a.is_active = a.id == account_id;
                if a.is_active {
                    if host.is_some() {
                        a.host.clone_from(&host);
                    }
                    if unix_user.is_some() {
                        a.unix_user.clone_from(&unix_user);
                    }
                    a.enrollment_state = EnrollmentState::Selected;
                } else if a.enrollment_state == EnrollmentState::Selected {
                    a.enrollment_state = EnrollmentState::Verified;
                }
            }
        }
        AccountCommand::SelectPersistentAccount { account_id, .. } => {
            if !find_account(snapshot, &account_id)?.enabled {
                return Err(reject(409, "fixture account is disabled"));
            }
            for a in &mut snapshot.accounts {
                a.is_preferred = a.id == account_id;
            }
        }
        AccountCommand::SetEnabled { account_id, enabled } => {
            let account = find_account(snapshot, &account_id)?;
            account.enabled = enabled;
            if !enabled {
                account.is_active = false;
                account.is_preferred = false;
                account.is_next_pick = false;
                if account.enrollment_state == EnrollmentState::Selected {
                    account.enrollment_state = EnrollmentState::Verified;
                }
            }
            account.status =
                AccountStatus::resolve(enabled, account.cooldown_until, at, account.is_next_pick, account.status_message.is_some());
        }
        AccountCommand::SetPriority { account_id, priority } => find_account(snapshot, &account_id)?.priority = priority,
        AccountCommand::SetWeight { account_id, weight } => find_account(snapshot, &account_id)?.weight = weight,
        AccountCommand::SetNote { account_id, note } => find_account(snapshot, &account_id)?.note = note,
    }
    complete(json!({"fixture": true, "applied": true}))
}

fn apply_routing(state: &mut State, intent: &ProviderIntent, command: RoutingCommand, at: DateTime<Utc>) -> PortResult<ProviderDispatch> {
    let revision = state.snapshot.routing_status.as_ref().expect("fixture routing").revision;
    if intent.expected_revision.is_some_and(|expected| expected != revision) {
        return Err(reject(409, "fixture routing revision changed"));
    }
    let allowed_account = match &command {
        RoutingCommand::SetPoolOverride { account_id } => account_id.as_deref(),
        RoutingCommand::SetRoutePolicy { policy } => policy.preferred_account_id.as_deref(),
        RoutingCommand::MoveConversation { target_account_id, .. } => Some(target_account_id.as_str()),
        _ => None,
    };
    if let Some(id) = allowed_account
        && !find_account(&mut state.snapshot, id)?.enabled
    {
        return Err(reject(409, "fixture account is disabled"));
    }
    let routing = state.snapshot.routing_status.as_mut().expect("fixture routing");
    let runtime = routing.runtime.as_mut().expect("fixture proxy");
    let undo_wiring = matches!(&command, RoutingCommand::UndoClientWiring { .. });
    let preference_changed = matches!(&command, RoutingCommand::SetRoutePolicy { .. } | RoutingCommand::SetPoolOverride { .. });
    let message = match command {
        RoutingCommand::ControlProxy { action } => {
            runtime.running = action != ProxyAction::Stop;
            "Fixture proxy state changed"
        }
        RoutingCommand::UpdateProxyConfig { port, lan_bind, auto_start, debug_logs } => {
            if let Some(port) = port {
                if port == 0 {
                    return Err(reject(400, "fixture proxy port must be nonzero"));
                }
                runtime.port = Some(port);
                runtime.endpoint = Some(format!("http://127.0.0.1:{port}"));
            }
            runtime.lan_bind = lan_bind.unwrap_or(runtime.lan_bind);
            runtime.auto_start = auto_start.unwrap_or(runtime.auto_start);
            runtime.debug_logs = debug_logs.unwrap_or(runtime.debug_logs);
            "Fixture proxy configuration updated"
        }
        RoutingCommand::SetRoutePolicy { policy } => {
            routing.policy = policy;
            "Fixture routing policy updated"
        }
        RoutingCommand::SetPoolOverride { account_id } => {
            routing.policy.preferred_account_id = account_id;
            "Fixture preferred account updated"
        }
        RoutingCommand::MoveConversation { session_id, target_account_id, include_subagents } => {
            if include_subagents {
                state.sessions.insert(format!("{session_id}-subagent"), target_account_id.clone());
            }
            state.sessions.insert(session_id, target_account_id);
            "Fixture conversation moved"
        }
        RoutingCommand::ApplyClientWiring { client_id }
        | RoutingCommand::ReapplyClientWiring { client_id }
        | RoutingCommand::UndoClientWiring { client_id } => {
            let client = routing.clients.iter_mut().find(|c| c.client_id == client_id).ok_or(PortError::NotFound)?;
            client.state = if undo_wiring { ClientWiringState::NotApplied } else { ClientWiringState::Applied };
            client.backup_available = !undo_wiring;
            client.managed_by = (!undo_wiring).then_some("fixture".into());
            "Fixture client wiring updated"
        }
        RoutingCommand::RegenerateClientKey => {
            runtime.client_key_hint = Some("••••new!".into());
            "Fixture client key hint rotated"
        }
        RoutingCommand::UnlockOutsideTool { lockout_id } => {
            let index = routing.lockouts.iter().position(|l| l.id == lockout_id).ok_or(PortError::NotFound)?;
            routing.lockouts.remove(index);
            "Fixture client lockout cleared"
        }
    };
    if preference_changed {
        for account in &mut state.snapshot.accounts {
            account.is_preferred = routing.policy.preferred_account_id.as_ref() == Some(&account.id);
        }
    }
    runtime.updated_at = at;
    routing.revision += 1;
    state.snapshot.proxy_logs.push(log(at, message));
    routing.recent_logs = state.snapshot.proxy_logs.clone();
    complete(json!({"fixture": true, "applied": true, "revision": routing.revision, "proxy_running": runtime.running}))
}

fn apply_probe(state: &mut State, intent: &ProviderIntent, command: ProbeCommand, at: DateTime<Utc>) -> PortResult<ProviderDispatch> {
    let (message, receipt) = match command {
        ProbeCommand::ConfigureCanarySchedule { schedule } => {
            if schedule.enabled && ![6, 12, 24].contains(&schedule.interval_hours) {
                return Err(reject(400, "fixture schedule requires a 6, 12, or 24 hour interval"));
            }
            state.schedule = schedule;
            (
                "Fixture canary schedule updated",
                json!({"fixture": true, "enabled": state.schedule.enabled, "targets": state.schedule.targets.len()}),
            )
        }
        ProbeCommand::RunCanary { account_id, .. } => {
            if let Some(id) = account_id {
                find_account(&mut state.snapshot, &id)?.success_count += 1;
            }
            (
                "Fixture canary completed",
                json!({"fixture": true, "run_id": intent.operation_id, "prompts_run": 3, "prompts_passed": 3,
                "mean_thinking_tokens": 704.0, "mean_ttft_ms": 420.0, "ran_at": at}),
            )
        }
        ProbeCommand::RunThinkingTest { sample_count, compare_model_id, compare_effort, .. } => {
            if sample_count == 0 {
                return Err(reject(400, "fixture thinking test requires samples"));
            }
            (
                "Fixture thinking test completed",
                json!({"fixture": true, "run_id": intent.operation_id, "sample_count": sample_count,
                "mean_thinking_tokens": 704.0, "comparison_mean_thinking_tokens": (compare_model_id.is_some() || compare_effort.is_some()).then_some(518.0),
                "mean_ttft_ms": 420.0, "ran_at": at}),
            )
        }
        ProbeCommand::ReplayTextEval { case_id, account_id, .. } => {
            if let Some(id) = account_id {
                find_account(&mut state.snapshot, &id)?.success_count += 1;
            }
            (
                "Fixture eval replay completed",
                json!({"fixture": true, "run_id": intent.operation_id, "case_id": case_id, "outcome": "ok",
                "thinking_tokens": 502, "output_tokens": 689, "ran_at": at}),
            )
        }
    };
    state.snapshot.proxy_logs.push(log(at, message));
    if let Some(routing) = &mut state.snapshot.routing_status {
        routing.recent_logs = state.snapshot.proxy_logs.clone();
    }
    complete(receipt)
}

pub struct ReadOnlyFixtureModelProvider {
    descriptor: ProviderDescriptor,
}

impl ReadOnlyFixtureModelProvider {
    pub fn new(id: &str, label: &str, family: &str) -> Self {
        let mut descriptor = descriptor(id, label, family, AdapterMode::Fixture);
        descriptor.health = ProviderHealth::Healthy;
        Self { descriptor }
    }
}

#[async_trait]
impl ModelProvider for ReadOnlyFixtureModelProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }
    fn facets(&self) -> ProviderFacets<'_> {
        ProviderFacets::observation_only(self.descriptor.evidence_kinds.clone())
    }
    async fn collect(&self) -> PortResult<Option<ObservationBatch>> {
        let at = Utc::now();
        let mut account = account(&self.descriptor, "readonly", "Observed subscription");
        account.last_observed_at = Some(at);
        account.enrollment_state = EnrollmentState::Verified;
        account.windows = [
            ("daily", "Daily · measured zero", Some(0.0), QuotaObservationState::Measured, QuotaWindowKind::Daily),
            ("monthly", "Monthly · never observed", None, QuotaObservationState::NeverObserved, QuotaWindowKind::Monthly),
            ("preview", "Preview · unavailable", None, QuotaObservationState::Unavailable, QuotaWindowKind::ModelScoped),
        ]
        .into_iter()
        .map(|(key, label, used, state, kind)| {
            let mut window = percent_window(key, label, used, at);
            window.observation_state = state;
            window.kind = kind;
            if state == QuotaObservationState::NeverObserved {
                window.observed_at = None;
            }
            window
        })
        .collect();
        Ok(Some(batch(&self.descriptor, "fixture-host", vec![account], at)))
    }
}
