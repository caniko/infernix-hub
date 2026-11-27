use std::collections::BTreeMap;

use super::{dashboard::account_name, *};

#[component]
pub(super) fn Routing(providers: Vec<ProviderSummary>, stale: bool) -> Element {
    let running = use_signal(BTreeMap::<String, bool>::new);
    let descriptors = providers.iter().map(|provider| provider.descriptor.clone()).collect::<Vec<_>>();
    let (mut capable, unsupported): (Vec<_>, Vec<_>) = providers.into_iter().partition(has_routing);
    capable.sort_by_key(|p| !running.read().get(&p.descriptor.id).copied().unwrap_or_default());
    let unsupported = unsupported.into_iter().map(|p| p.descriptor.label).collect::<Vec<_>>().join(", ");
    rsx! {
        for provider in capable {
            RouterCard { key: "{provider.descriptor.id}", provider, running }
        }
        if !unsupported.is_empty() {
            aside { class: "severity-row form-help",
                strong { "No proxy or routing controls: " }
                "{unsupported}"
            }
        }
        if !descriptors.is_empty() {
            ProviderAdapterInspector { providers: descriptors, stale }
        }
    }
}

#[component]
fn ProviderAdapterInspector(providers: Vec<ProviderDescriptor>, stale: bool) -> Element {
    let now = Utc::now();
    rsx! {
        section { class: "glass-card stack",
            h2 { "Provider adapter inspector" }
            div { class: "adapter-inspector-grid",
                for provider in providers {
                    {
                        let (health, tone) = match provider.health {
                            ProviderHealth::Healthy if stale => ("Healthy (last known)", "muted"),
                            ProviderHealth::Healthy => ("Healthy", "ok"),
                            ProviderHealth::Degraded => ("Degraded", "warn"),
                            ProviderHealth::Offline => ("Offline", "bad"),
                            ProviderHealth::Unobserved => ("Not observed", "muted"),
                        };
                        let capability_count = provider.capabilities.len();
                        rsx! {
                            article { key: "{provider.id}", class: "adapter-inspector-card provider-inset stack",
                                div { class: "row",
                                    h3 { "{provider.label}" }
                                    Chip { label: health.to_string(), tone: Some(tone.to_string()) }
                                }
                                p { class: "form-help", "ID: {provider.id} · Family: {provider.family}" }
                                if let Some(router) = &provider.router_id { p { class: "form-help", "Router ID: {router}" } }
                                if let Some(message) = &provider.status_message { p { "{message}" } }
                                p { "Mode: {adapter_mode_label(provider.mode)} · Verification: {verification_label(provider.verification)}" }
                                p { "Last observed: "
                                    if let Some(observed) = provider.last_observed_at {
                                        time { datetime: observed.to_rfc3339(), title: observed.to_rfc3339(), "{date(Some(observed))}" }
                                        " · {format_relative_age(observed, now)}"
                                    } else {
                                        "Not observed"
                                    }
                                }
                                div { class: "row",
                                    strong { "Evidence" }
                                    if provider.evidence_kinds.is_empty() {
                                        span { class: "muted", "None" }
                                    }
                                    for &kind in &provider.evidence_kinds {
                                        span { key: "{kind:?}", class: "chip muted", "{evidence_kind_label(kind)}" }
                                    }
                                }
                                details {
                                    summary { "Supported capabilities ({capability_count})" }
                                    if provider.capabilities.is_empty() {
                                        p { class: "muted", "None" }
                                    } else {
                                        ul {
                                            for cap in &provider.capabilities {
                                                li { key: "{cap:?}", "{capability_label(*cap)}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn capability_label(capability: ProviderCapability) -> String {
    let wire_name = serde_json::to_value(capability).unwrap_or_default();
    wire_name.as_str().unwrap_or("Unknown").replace('_', " ").replace("oauth", "OAuth")
}

fn has_routing(provider: &ProviderSummary) -> bool {
    provider.descriptor.router_id.is_some()
        || provider.descriptor.capabilities.iter().any(|cap| {
            matches!(
                cap,
                ProviderCapability::ReadProxyStatus
                    | ProviderCapability::ControlProxy
                    | ProviderCapability::SetRoutePolicy
                    | ProviderCapability::ReadClientWiring
                    | ProviderCapability::ManageLockouts
                    | ProviderCapability::ReadProxyLogs
                    | ProviderCapability::ConfigureProxy
                    | ProviderCapability::SetPoolOverride
                    | ProviderCapability::MoveConversation
                    | ProviderCapability::ManageClientKey
                    | ProviderCapability::ApplyClientWiring
                    | ProviderCapability::UndoClientWiring
            )
        })
}

#[component]
fn RouterCard(provider: ProviderSummary, mut running: Signal<BTreeMap<String, bool>>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let id = segment(&provider.descriptor.id);
    let provider_id = provider.descriptor.id.clone();
    let data = use_resource(move || {
        let (id, client) = (id.clone(), client.clone());
        let provider_id = provider_id.clone();
        let _ = (panel.epoch)();
        async move {
            let view = timed(client.provider_routing(&path_id(id)?)).await?;
            let is_running = view.runtime.as_ref().is_some_and(|runtime| runtime.running);
            if running.peek().get(&provider_id).copied() != Some(is_running) {
                running.write().insert(provider_id, is_running);
            }
            Ok(view)
        }
    });
    let result = data.read();
    rsx! {
        section { class: "glass-card stack",
            h2 { "{provider.descriptor.label} · Routing & Proxy" }
            {read_view(result.as_ref(), |view| rsx! {
                RouterDetails { key: "{view.revision}", provider: provider.clone(), view: view.clone() }
            })}
            if panel.operator() && provider.descriptor.capabilities.contains(&ProviderCapability::ReadProxyLogs) {
                details {
                    summary { "Redacted proxy logs" }
                    Logs { provider_id: provider.descriptor.id.clone() }
                }
            }
        }
    }
}

#[component]
fn RouterDetails(provider: ProviderSummary, view: RoutingView) -> Element {
    let revision = view.revision;
    let panel = use_context::<PanelState>();
    let can_rotate = panel.operator() && supports(&provider, None, ProviderCapability::ManageClientKey);
    let running = view.runtime.as_ref().map(|r| r.running);
    rsx! {
        div { class: "provider-inset stack",
            h3 { "Proxy runtime" }
            if let Some(runtime) = &view.runtime {
                div { class: "row",
                    Chip { label: if runtime.running { "Running".to_string() } else { "Stopped".to_string() }, tone: Some(if runtime.running { "ok".into() } else { "muted".into() }) }
                    span { class: "form-help", "Observed " span { class: "nowrap-unit", "{fmt_dt(runtime.updated_at)}" } }
                }
                dl { class: "kv-grid content-bounded", style: "max-width: 46rem;",
                    div { class: "kv-item proxy-endpoint",
                        dt { class: "kv-label", "Endpoint" }
                        dd { class: "kv-value nowrap-unit",
                            if let Some(endpoint) = &runtime.endpoint { "{endpoint}" }
                            else if let Some(port) = runtime.port { "Port {port}" }
                            else { "—" }
                        }
                    }
                    for (label, value) in [
                        ("LAN access", if runtime.lan_bind { "Enabled" } else { "Off" }.into()),
                        ("Build", runtime.build_version.clone().unwrap_or_else(|| "—".into())),
                        ("Client Key", runtime.client_key_hint.clone().unwrap_or_else(|| "—".into())),
                        ("Crashes", runtime.crash_restarts.to_string()),
                    ] {
                        div { key: "{label}", class: "kv-item", dt { class: "kv-label", "{label}" } dd { class: "kv-value", "{value}" } }
                    }
                }
                if let Some(owner) = &runtime.shared_profile_owner { p { class: "form-help", "Shared profile owner: {owner}" } }
                if let Some(error) = &runtime.last_error { p { class: "severity-row severity-warn", role: "status", "{error}" } }
            } else { p { class: "muted", "Proxy runtime not observed." } }
            div { class: "row",
                if running != Some(true) {
                    OperationButton { provider: provider.clone(), label: "Start", tone: "primary",
                        command: ProviderCommand::Routing(RoutingCommand::ControlProxy { action: ProxyAction::Start }) }
                }
                for (action, label, tone) in [(ProxyAction::Restart, "Restart", "secondary"), (ProxyAction::Stop, "Stop", "warning")] {
                    OperationButton { key: "{label}", provider: provider.clone(), label, tone, confirm: true, disabled: running == Some(false),
                        command: ProviderCommand::Routing(RoutingCommand::ControlProxy { action }) }
                }
            }
            if can_rotate {
                div { class: "row",
                    OperationButton { provider: provider.clone(), label: "Regenerate client key", tone: "danger", confirm: true,
                        disabled: view.runtime.as_ref().is_some_and(|r| r.shared_profile_owner.is_some()),
                        command: ProviderCommand::Routing(RoutingCommand::RegenerateClientKey) }
                    span { class: "form-help", "After regeneration, reconnect existing clients with the new key." }
                }
            }
        }
        RoutingPolicyView { provider: provider.clone(), policy: view.policy, revision }
        if panel.operator() && supports(&provider, None, ProviderCapability::ConfigureProxy) && let Some(runtime) = &view.runtime {
            details { class: "provider-inset", summary { "Proxy configuration" }
                ProxyConfig { provider: provider.clone(), runtime: runtime.clone(), revision }
            }
        }
        MoveConversation { provider: provider.clone(), session_id: String::new() }
        if !view.clients.is_empty() || provider.descriptor.capabilities.contains(&ProviderCapability::ReadClientWiring) {
            div { class: "provider-inset stack",
                h3 { "Client wiring" }
                if view.clients.is_empty() { p { class: "muted", "No client wiring observed." } }
                for client in &view.clients {
                    article { key: "{client.client_id}", class: "quota-window",
                        div { class: "row",
                            strong { "{client.display_name}" }
                            span { "{client_wiring_label(client.state)} · Backup: " if client.backup_available { "Available" } else { "None" } }
                        }
                        if let Some(owner) = &client.managed_by { p { "Managed by {owner}" } }
                        if let Some(detail) = &client.detail { p { "{detail}" } }
                        if let Some(instructions) = &client.instructions { pre { class: "raw-json", "{instructions}" } }
                        if client.state != ClientWiringState::ExternallyManaged {
                            div { class: "row",
                                OperationButton { provider: provider.clone(), label: "Apply", confirm: true, disabled: client.state != ClientWiringState::NotApplied,
                                    command: ProviderCommand::Routing(RoutingCommand::ApplyClientWiring { client_id: client.client_id.clone() }) }
                                OperationButton { provider: provider.clone(), label: "Undo", confirm: true, disabled: !client.backup_available,
                                    command: ProviderCommand::Routing(RoutingCommand::UndoClientWiring { client_id: client.client_id.clone() }) }
                                OperationButton { provider: provider.clone(), label: "Reapply", confirm: true, disabled: client.state == ClientWiringState::NotApplied,
                                    command: ProviderCommand::Routing(RoutingCommand::ReapplyClientWiring { client_id: client.client_id.clone() }) }
                            }
                        }
                    }
                }
            }
        }
        if !view.lockouts.is_empty() || provider.descriptor.capabilities.contains(&ProviderCapability::ManageLockouts) {
            div { class: "provider-inset stack",
                h3 { "Outside-tool lockouts" }
                if view.lockouts.is_empty() { p { class: "muted", "No lockouts reported." } }
                for lock in &view.lockouts {
                    div { key: "{lock.id}", class: "row",
                        span { "{lock.tool_name} · {lock.reason} · {fmt_dt(lock.locked_at)}" }
                        OperationButton { provider: provider.clone(), label: "Unlock", confirm: true,
                            command: ProviderCommand::Routing(RoutingCommand::UnlockOutsideTool { lockout_id: lock.id.clone() }) }
                    }
                }
            }
        }
    }
}

#[component]
fn ProxyConfig(provider: ProviderSummary, runtime: ProxyRuntimeStatus, revision: i64) -> Element {
    let mut port = use_signal(|| runtime.port.map(|n| n.to_string()).unwrap_or_default());
    let mut lan = use_signal(|| runtime.lan_bind);
    let mut auto = use_signal(|| runtime.auto_start);
    let lan_help = format!("proxy-lan-help-{}", use_hook(Uuid::new_v4));
    let port_num = port().parse::<u16>().ok().filter(|n| *n > 0);
    rsx! { div { class: "row content-bounded",
        Field { label: "Port", kind: "number", value: port(), change: move |v| port.set(v) }
        div { class: "form-field", style: "max-width: 15rem;",
            Check { label: "Allow LAN", value: lan(), description_id: lan_help.clone(), change: move |v| lan.set(v) }
            p { class: "form-help", id: lan_help, "Let other devices on your local network connect to the proxy." }
        }
        Check { label: "Auto start", value: auto(), change: move |v| auto.set(v) }
        OperationButton { provider, label: "Save proxy configuration", revision, confirm: lan() && !runtime.lan_bind,
            disabled: !port().is_empty() && port_num.is_none(),
            command: ProviderCommand::Routing(RoutingCommand::UpdateProxyConfig {
                port: port_num, lan_bind: Some(lan()), auto_start: Some(auto()), debug_logs: None,
            }) }
    } }
}

const STRATEGIES: &[RouteStrategy] = &[
    RouteStrategy::TierAscending,
    RouteStrategy::CapacityBalanced,
    RouteStrategy::SoonestReset,
    RouteStrategy::FillFirst,
    RouteStrategy::RoundRobin,
    RouteStrategy::WeightedRoundRobin,
];
const ORDERS: &[(ApiKeyOrder, &str)] =
    &[(ApiKeyOrder::KeysLast, "Keys last"), (ApiKeyOrder::KeysFirst, "Keys first"), (ApiKeyOrder::SubscriptionsOnly, "Subscriptions only")];

#[component]
fn RoutingPolicyView(provider: ProviderSummary, policy: RoutingPolicy, revision: i64) -> Element {
    let mut draft = use_signal(|| policy.clone());
    let mut affinity = use_signal(|| policy.session_affinity_secs.to_string());
    let mut cooldown = use_signal(|| policy.failover_cooldown_secs.to_string());
    let mut near_reset = use_signal(|| policy.near_reset_wait_secs.to_string());
    let mut first_byte = use_signal(|| policy.first_byte_timeout_secs.to_string());
    let mut preferred = use_signal(|| policy.preferred_account_id.clone().unwrap_or_default());
    let help_id = use_hook(Uuid::new_v4);
    let strategy_help = format!("routing-strategy-help-{help_id}");
    let key_order_help = format!("routing-key-order-help-{help_id}");
    let near_reset_help = format!("routing-near-reset-help-{help_id}");
    let saved_strategy = route_strategy_label(policy.strategy);
    let saved_key_order = api_key_order_label(policy.api_key_order);
    let (a, c, n, f) = (affinity().parse(), cooldown().parse(), near_reset().parse(), first_byte().parse());
    let valid = a.is_ok() && c.is_ok() && n.is_ok() && f.is_ok();
    let policy = RoutingPolicy {
        session_affinity_secs: a.unwrap_or_default(),
        failover_cooldown_secs: c.unwrap_or_default(),
        near_reset_wait_secs: n.unwrap_or_default(),
        first_byte_timeout_secs: f.unwrap_or_default(),
        ..draft()
    };
    let current = policy.clone();
    let key_order_caption = match current.api_key_order {
        ApiKeyOrder::KeysLast => "Prefer subscription logins; fall back to API keys last.",
        ApiKeyOrder::KeysFirst => "Try API keys before subscription logins.",
        ApiKeyOrder::SubscriptionsOnly => "Use subscription logins without falling back to API keys.",
    };
    let mask = use_context::<PanelState>().settings.read().mask_emails;
    let mut options = vec![(String::new(), "Automatic".into())];
    options.extend(
        provider.accounts.iter().filter(|a| a.enabled || preferred.read().as_str() == a.id).map(|a| (a.id.clone(), account_name(a, mask))),
    );
    rsx! {
        section { class: "provider-inset stack",
            h3 { "Routing policy" }
            p { class: "form-help", "Revision {revision} · current strategy: {saved_strategy} · key order: {saved_key_order}" }
            if supports(&provider, None, ProviderCapability::SetRoutePolicy) {
                div { class: "form-grid-2 content-bounded", style: "align-items: start;",
                    div { class: "form-field",
                        Choice { label: "Strategy", value: format!("{:?}", current.strategy), description_id: strategy_help.clone(),
                            options: STRATEGIES.iter().map(|&v| (format!("{v:?}"), route_strategy_label(v).to_string())).collect(),
                            change: move |key: String| if let Some(&v) = STRATEGIES.iter().find(|v| format!("{v:?}") == key) { draft.write().strategy = v; } }
                        p { class: "form-help", id: strategy_help, "{super::route_strategy_description(current.strategy)}" }
                    }
                    div { class: "form-field",
                        Choice { label: "Key order", value: format!("{:?}", current.api_key_order), description_id: key_order_help.clone(),
                            options: ORDERS.iter().map(|(v, label)| (format!("{v:?}"), label.to_string())).collect(),
                            change: move |key: String| if let Some((v, _)) = ORDERS.iter().find(|(v, _)| format!("{v:?}") == key) { draft.write().api_key_order = *v; } }
                        p { class: "form-help", id: key_order_help, "{key_order_caption}" }
                    }
                    Field { label: "Session affinity (seconds)", kind: "number", value: affinity(), change: move |v| affinity.set(v) }
                    Field { label: "Failover cooldown (seconds)", kind: "number", value: cooldown(), change: move |v| cooldown.set(v) }
                    div { class: "form-field",
                        Field { label: "Near-reset wait (seconds)", kind: "number", value: near_reset(), description_id: near_reset_help.clone(),
                            change: move |v| near_reset.set(v) }
                        p { class: "form-help", id: near_reset_help, "Maximum wait for a quota reset, where supported. 0 skips the wait." }
                    }
                    Field { label: "First-byte timeout (seconds)", kind: "number", value: first_byte(), change: move |v| first_byte.set(v) }
                }
                div { class: "row",
                    Check { label: "Switch on server error", value: current.switch_on_server_error, change: move |v| draft.write().switch_on_server_error = v }
                    OperationButton { provider: provider.clone(), label: "Save policy", tone: "primary", revision, disabled: !valid,
                        command: ProviderCommand::Routing(RoutingCommand::SetRoutePolicy { policy }) }
                }
            }
        }
        if supports(&provider, None, ProviderCapability::SetPoolOverride) {
            section { class: "provider-inset stack",
                h3 { "Account override" }
                p { class: "form-help", "Choose Automatic to follow the routing policy." }
                div { class: "row content-bounded",
                    Choice { label: "Preferred pool account", value: preferred(), options, change: move |v| preferred.set(v) }
                    OperationButton { provider, label: "Apply override", revision, confirm: true,
                        command: ProviderCommand::Routing(RoutingCommand::SetPoolOverride { account_id: optional(&preferred()) }) }
                }
            }
        }
    }
}

#[component]
pub(super) fn MoveConversation(provider: ProviderSummary, session_id: String) -> Element {
    let mut session = use_signal(|| session_id.clone());
    let mut target = use_signal(String::new);
    let mut include = use_signal(|| false);
    let mask = use_context::<PanelState>().settings.read().mask_emails;
    let mut accounts = vec![(String::new(), "Select an account".into())];
    accounts.extend(provider.accounts.iter().filter(|a| a.enabled).map(|a| (a.id.clone(), account_name(a, mask))));
    if !supports(&provider, None, ProviderCapability::MoveConversation) {
        return rsx! {};
    }
    rsx! { div { class: "provider-inset stack",
        h3 { "Move Conversation" }
        p { class: "muted", "Reassign one session; moving can sacrifice its warm cache." }
        div { class: "row",
            if session_id.is_empty() {
                Field { label: "Session ID", value: session(), change: move |v| session.set(v) }
            } else { span { class: "mono", "{session_id}" } }
            Choice { label: "Target account", value: target(), options: accounts, change: move |v| target.set(v) }
            Check { label: "Include subagents", value: include(), change: move |v| include.set(v) }
            OperationButton { provider, label: "Move Conversation", confirm: true,
                disabled: optional(&session()).is_none() || target().is_empty(),
                command: ProviderCommand::Routing(RoutingCommand::MoveConversation { session_id: session().trim().into(), target_account_id: target(), include_subagents: include() }) }
        }
    } }
}

#[component]
fn Logs(provider_id: String) -> Element {
    let client = use_context::<HubClient>();
    let mut loaded = use_signal(Loaded::<Vec<ProxyLogEntry>>::default);
    let mut live = use_signal(|| false);
    let mut follow = use_signal(|| true);
    let mut cleared = use_signal(|| None::<DateTime<Utc>>);
    let id = segment(&provider_id);
    use_resource(move || {
        let (id, client) = (id.clone(), client.clone());
        let live = live();
        async move {
            if live {
                let id = match path_id(id) {
                    Ok(id) => id,
                    Err(error) => {
                        loaded.write().failed(error);
                        return;
                    }
                };
                loop {
                    match timed(client.provider_logs(&id)).await {
                        Ok(rows) => loaded.set(Loaded::ready(rows)),
                        Err(error) => loaded.write().failed(error),
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });
    let mut node = use_signal(|| None::<Event<MountedData>>);
    use_effect(move || {
        let _ = loaded.read();
        if follow()
            && let Some(element) = node.cloned()
        {
            spawn(async move {
                let _ = element.scroll_to(ScrollBehavior::Instant).await;
            });
        }
    });
    let logs = loaded.cloned();
    let rows = logs.data.unwrap_or_default();
    rsx! { div { class: "stack",
        div { class: "row",
            Check { label: "Live stream", value: live(), change: move |v| live.set(v) }
            Check { label: "Follow tail", value: follow(), change: move |v| follow.set(v) }
            button { class: "btn btn-glass", onclick: move |_| cleared.set(rows.iter().map(|row| row.at).max()), "Clear view" }
        }
        if let Some(error) = logs.error { ErrorCard { code: error.code, message: error.message } }
        if !live() { p { class: "muted", "Live updates are paused." } }
        div { class: "provider-logs", tabindex: "0", aria_label: "Redacted proxy log entries",
            for entry in loaded.read().data.iter().flatten().filter(|r| cleared().is_none_or(|at| r.at > at)) {
                p { class: "mono",
                    span { class: "chip", "{entry.stream} · {entry.level}" }
                    " {fmt_dt(entry.at)} · {entry.message}"
                }
            }
            div { onmounted: move |e| node.set(Some(e)) }
        }
    } }
}
