use std::{
    collections::{BTreeSet, VecDeque},
    env,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use contracts::{client::HubClient, providers::*};
use serde_json::json;
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::{
    Proxy,
    management::{self, Changes, Failure},
    pool::{Lane, Pool},
};

const FAMILIES: [&str; 3] = ["claude", "antigravity", "opencode"];
const ROUTER_ID: &str = "fleetix-proxy";
const DEADLINE: Duration = Duration::from_secs(10);

pub async fn poll_quota(proxy: Arc<Proxy>) {
    if proxy.config.discover_local {
        use std::process::Stdio;
        use tokio::process::Command;
        let read = Command::new("opencode")
            .args(["auth", "list", "--format", "json"])
            .env_clear()
            .envs(
                ["PATH", "HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "OPENCODE_DB"]
                    .into_iter()
                    .filter_map(|key| env::var_os(key).map(|value| (key, value))),
            )
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        if let Ok(Ok(output)) = timeout(Duration::from_secs(3), read).await
            && output.status.success()
            && output.stdout.len() <= 2 * 1024 * 1024
            && let Ok(raw) = serde_json::from_slice(&output.stdout)
        {
            crate::pool::add_opencode(&mut proxy.state.write().await.pool, &raw);
        }
    }
    let mut ticks = interval(Duration::from_secs(300));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        let _ = management::refresh_selected(&proxy, None, None).await;
    }
}

pub async fn run(proxy: Arc<Proxy>, url: String, token: String) {
    let hub = HubClient::new(url, token);
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "localhost".into());
    let user = env::var("USER").unwrap_or_else(|_| "unknown".into());
    let claim =
        ClaimOperationsIn { host: host.clone(), unix_user: user.clone(), provider_ids: FAMILIES.map(str::to_owned).into(), limit: 10 };
    let mut receipts = VecDeque::new();
    let mut tick = interval(Duration::from_secs(3));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut observation_due = true;
    let mut count = 0_u8;
    loop {
        tick.tick().await;
        count = (count + 1) % 10;
        if observation_due || count == 0 {
            let batches = observations(&proxy, &host, &user).await;
            let results = futures_util::future::join_all(
                batches.iter().map(|batch| timeout(DEADLINE, hub.ingest_provider_observations(&batch.provider_id, batch))),
            )
            .await;
            if results.iter().any(|r| !matches!(r, Ok(Ok(_)))) {
                proxy.log("warn", "hub observation delivery failed").await;
            }
            observation_due = false;
        }

        let exchanges = proxy.state.read().await.activity.iter().take(64).cloned().collect::<Vec<_>>();
        if !exchanges.is_empty() {
            let ids: BTreeSet<_> = exchanges.iter().map(|e| e.source_exchange_id.clone()).collect();
            let batch = ActivityIngestBatch { source_host: host.clone(), source_user: user.clone(), exchanges };
            let delivered = timeout(DEADLINE, hub.ingest_activity(&batch)).await;
            if matches!(delivered, Ok(Ok(ack)) if ack.accepted.saturating_add(ack.deduplicated) == ids.len()) {
                proxy.state.write().await.activity.retain(|e| !ids.contains(&e.source_exchange_id));
            } else {
                proxy.log("warn", "hub activity delivery failed").await;
            }
        }

        if receipts.is_empty() {
            match timeout(DEADLINE, hub.claim_provider_operations(&claim)).await {
                Ok(Ok(ops)) => {
                    for op in ops.into_iter().take(claim.limit as usize) {
                        let result = execute(proxy.clone(), &op).await;
                        let receipt = match result {
                            Ok(value) => OperationReceiptIn { status: OperationStatus::Succeeded, receipt: Some(value), error: None },
                            Err(Failure(_, message)) => {
                                OperationReceiptIn { status: OperationStatus::Failed, receipt: None, error: Some(message.into()) }
                            }
                        };
                        receipts.push_back((op.id, receipt));
                        observation_due = true;
                    }
                }
                _ => proxy.log("warn", "hub operation claim failed").await,
            }
        }
        while let Some((id, receipt)) = receipts.front() {
            // Keep the result until delivered; the Hub can return dispatched operations again.
            if timeout(DEADLINE, hub.report_provider_operation_receipt(*id, receipt)).await.is_ok_and(|r| r.is_ok()) {
                receipts.pop_front();
            } else {
                proxy.log("warn", "hub operation receipt delivery failed").await;
                break;
            }
        }
    }
}

pub(crate) async fn observations(proxy: &Proxy, host: &str, user: &str) -> Vec<ObservationBatch> {
    use ProviderCapability::*;
    let state = proxy.state.read().await;
    let pool = &state.pool;
    let at = Utc::now();
    let clients = management::client_status(proxy);
    FAMILIES
        .into_iter()
        .map(|family| {
            let capabilities: BTreeSet<_> = [
                RefreshQuota,
                SetEnabled,
                SetPriority,
                SetWeight,
                SetNote,
                ReadProxyStatus,
                ReadProxyLogs,
                SetRoutePolicy,
                SetPoolOverride,
                MoveConversation,
                ReadClientWiring,
                ApplyClientWiring,
                UndoClientWiring,
            ]
            .into();
            let lanes =
                [Lane::Claude, Lane::Codex, Lane::Google, Lane::Go].into_iter().filter(|lane| lane.family() == family).collect::<Vec<_>>();
            let picks: BTreeSet<_> = lanes.iter().filter_map(|lane| pool.preview(*lane)).collect();
            let accounts: Vec<_> = pool
                .accounts
                .values()
                .filter(|a| a.lane.family() == family)
                .map(|a| {
                    let mut account_caps = capabilities.clone();
                    if a.api_key {
                        account_caps.remove(&RefreshQuota);
                    }
                    let is_next = picks.contains(&a.id);
                    ModelAccount {
                        id: a.hub_id(),
                        provider_id: family.into(),
                        source_account_id: a.id.clone(),
                        display_name: a.label.clone(),
                        email: None,
                        masked_email: a.email.as_deref().map(mask_email),
                        note: a.note.clone(),
                        plan: a.plan.clone(),
                        tier_multiplier: a.multiplier,
                        status: AccountStatus::resolve(!a.disabled, a.cooldown, at, is_next, a.probe_failed),
                        enabled: !a.disabled,
                        priority: a.priority,
                        weight: a.weight,
                        cooldown_until: a.cooldown,
                        status_message: a.probe_failed.then(|| "quota probe failed".into()),
                        is_active: false,
                        is_preferred: pool.preferred(a.lane) == Some(a.id.as_str()),
                        is_next_pick: is_next,
                        enrollment_state: if a.observed.is_some() { EnrollmentState::Verified } else { EnrollmentState::Deployed },
                        host: Some(host.into()),
                        unix_user: Some(user.into()),
                        credential_fingerprint: None,
                        last_used_at: a.last_used,
                        last_observed_at: a.observed,
                        capabilities: account_caps,
                        success_count: a.success,
                        failure_count: a.failure,
                        windows: a.windows.clone(),
                        reset_grant: a.grants.iter().max_by_key(|g| (g.applicable, g.available_count)).cloned(),
                        rolling_starter: None,
                    }
                })
                .collect();
            let models: BTreeSet<_> = pool.accounts.values().filter(|a| a.lane.family() == family).flat_map(|a| a.models.iter()).collect();
            let mut policy = pool.policy.clone();
            policy.preferred_account_id = accounts.iter().find(|a| a.is_preferred).map(|a| a.id.clone());
            ObservationBatch {
                provider_id: family.into(),
                provider_label: match family {
                    "claude" => "Claude",
                    "antigravity" => "Antigravity",
                    _ => "OpenCode",
                }
                .into(),
                provider_family: family.into(),
                source_host: host.into(),
                source_user: user.into(),
                collected_at: at,
                health: if accounts.iter().any(|a| a.status == AccountStatus::Error) {
                    ProviderHealth::Degraded
                } else {
                    ProviderHealth::Healthy
                },
                status_message: None,
                router_id: Some(ROUTER_ID.into()),
                capabilities,
                evidence_kinds: [
                    EvidenceKind::AccountInventory,
                    EvidenceKind::QuotaWindows,
                    EvidenceKind::ResetGrants,
                    EvidenceKind::RequestMetadata,
                    EvidenceKind::TraceEdges,
                ]
                .into(),
                accounts,
                quota_samples: vec![],
                reset_grants: vec![],
                key_groups: vec![],
                models: models
                    .into_iter()
                    .map(|id| DiscoveredModel {
                        id: id.clone(),
                        provider_id: family.into(),
                        family: Some(family.into()),
                        display_name: None,
                        group_id: None,
                        context_window: None,
                        supports_thinking: None,
                        discovered_at: None,
                    })
                    .collect(),
                routing_status: Some(RoutingView {
                    provider_id: family.into(),
                    router_id: Some(ROUTER_ID.into()),
                    revision: pool.revision,
                    policy,
                    clients: clients.clone(),
                    runtime: Some(ProxyRuntimeStatus {
                        router_id: ROUTER_ID.into(),
                        running: true,
                        endpoint: Some(proxy.config.url.clone()),
                        port: Some(proxy.config.bind.port()),
                        lan_bind: !proxy.config.bind.ip().is_loopback(),
                        auto_start: false,
                        debug_logs: false,
                        build_version: Some(env!("CARGO_PKG_VERSION").into()),
                        client_key_hint: None,
                        shared_profile_owner: None,
                        crash_restarts: 0,
                        last_error: None,
                        updated_at: at,
                    }),
                    lockouts: vec![],
                    recent_logs: state.logs.iter().cloned().collect(),
                    rolling_starter: RollingWindowStarterConfig::default(),
                }),
                proxy_logs: vec![],
            }
        })
        .collect()
}

fn scope(pool: &Pool, family: &str, id: &str) -> Result<Lane, Failure> {
    pool.find(id)
        .filter(|a| a.lane.family() == family)
        .map(|a| a.lane)
        .ok_or(Failure(axum::http::StatusCode::NOT_FOUND, "account not found for provider"))
}

pub(crate) async fn execute(proxy: Arc<Proxy>, op: &ProviderOperation) -> Result<serde_json::Value, Failure> {
    use axum::http::StatusCode;
    let family = op.provider_id.as_str();
    if !FAMILIES.contains(&family) {
        return Err(Failure(StatusCode::NOT_FOUND, "unknown provider"));
    }
    match &op.command {
        ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id }) => {
            let id = account_id.as_deref().or(op.target_account_id.as_deref());
            let errors = management::refresh_selected(&proxy, id, Some(family)).await?;
            if !errors.is_empty() {
                return Err(Failure(StatusCode::BAD_GATEWAY, "one or more quota probes failed"));
            }
        }
        ProviderCommand::Routing(RoutingCommand::ApplyClientWiring { client_id } | RoutingCommand::ReapplyClientWiring { client_id }) => {
            management::wire_client(proxy, client_id, false).await?
        }
        ProviderCommand::Routing(RoutingCommand::UndoClientWiring { client_id }) => management::wire_client(proxy, client_id, true).await?,
        command => {
            let pool = &mut proxy.state.write().await.pool;
            match command {
                ProviderCommand::Routing(RoutingCommand::SetRoutePolicy { policy }) => {
                    if let Some(id) = policy.preferred_account_id.as_deref() {
                        scope(pool, family, id)?;
                    }
                    management::update_policy(pool, policy.clone())?;
                }
                ProviderCommand::Routing(RoutingCommand::SetPoolOverride { account_id }) => {
                    if let Some(id) = account_id {
                        let lane = scope(pool, family, id)?;
                        management::override_account(pool, Some(lane), Some(id))?;
                    } else {
                        for lane in [Lane::Claude, Lane::Codex, Lane::Google, Lane::Go].into_iter().filter(|lane| lane.family() == family) {
                            management::override_account(pool, Some(lane), None)?;
                        }
                    }
                }
                ProviderCommand::Routing(RoutingCommand::MoveConversation { session_id, target_account_id, include_subagents }) => {
                    scope(pool, family, target_account_id)?;
                    let count = pool
                        .move_session(session_id, target_account_id, *include_subagents)
                        .map_err(|e| Failure(StatusCode::CONFLICT, e))?;
                    return Ok(json!({"applied":true, "bindings_moved":count}));
                }
                ProviderCommand::Account(command) => {
                    let (id, changes) = match command {
                        AccountCommand::SetEnabled { account_id, enabled } => {
                            (account_id, Changes { disabled: Some(!enabled), ..Default::default() })
                        }
                        AccountCommand::SetPriority { account_id, priority } => {
                            (account_id, Changes { priority: Some(*priority), ..Default::default() })
                        }
                        AccountCommand::SetWeight { account_id, weight } => {
                            (account_id, Changes { weight: Some(*weight), ..Default::default() })
                        }
                        AccountCommand::SetNote { account_id, note } => {
                            (account_id, Changes { note: Some(json!(note)), ..Default::default() })
                        }
                        _ => return Err(Failure(StatusCode::NOT_IMPLEMENTED, "unsupported account operation")),
                    };
                    scope(pool, family, id)?;
                    management::change(&proxy, pool, id, changes)?;
                }
                _ => return Err(Failure(StatusCode::NOT_IMPLEMENTED, "unsupported proxy operation")),
            }
        }
    }
    Ok(json!({"applied":true}))
}
