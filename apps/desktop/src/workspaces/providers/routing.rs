use super::{dashboard::account_name, *};

#[component]
pub(super) fn Routing(providers: Vec<ProviderSummary>) -> Element {
    rsx! { for provider in providers {
        RouterCard { key: "{provider.descriptor.id}", provider }
    } }
}

#[component]
fn RouterCard(provider: ProviderSummary) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let has_controls = provider.descriptor.router_id.is_some()
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
            )
        });
    let id = segment(&provider.descriptor.id);
    let data = use_resource(move || {
        let (id, client) = (id.clone(), client.clone());
        let _ = (panel.epoch)();
        async move { if has_controls { timed(client.provider_routing(&path_id(id)?)).await.map(Some) } else { Ok(None) } }
    });
    let result = data.read();
    rsx! {
        section { class: "glass-card stack",
            h2 { "{provider.descriptor.label} · Routing & Proxy" }
            if has_controls {
                {read_view(result.as_ref(), |view| rsx! {
                    if let Some(view) = view {
                        RouterDetails { key: "{view.revision}", provider: provider.clone(), view: view.clone() }
                    }
                })}
                if panel.operator() && provider.descriptor.capabilities.contains(&ProviderCapability::ReadProxyLogs) {
                    details {
                        summary { "Redacted proxy logs" }
                        Logs { provider_id: provider.descriptor.id.clone() }
                    }
                }
            } else {
                p { class: "muted", "No proxy or routing capabilities reported for this provider." }
            }
        }
    }
}

#[component]
fn RouterDetails(provider: ProviderSummary, view: RoutingView) -> Element {
    let revision = view.revision;
    rsx! {
        div { class: "provider-inset stack",
            h3 { "Proxy runtime" }
            if let Some(runtime) = &view.runtime {
                div { class: "row",
                    Chip { label: if runtime.running { "Running".to_string() } else { "Stopped".to_string() }, tone: Some(if runtime.running { "ok".into() } else { "muted".into() }) }
                    span { "Observed {fmt_dt(runtime.updated_at)}" }
                }
                p { {format!("Endpoint: {} · Port: {} · Build: {}", runtime.endpoint.as_deref().unwrap_or("—"), runtime.port.map_or_else(|| "—".to_string(), |p| p.to_string()), runtime.build_version.as_deref().unwrap_or("—"))} }
                p { "LAN bind: {runtime.lan_bind} · Client key: " {runtime.client_key_hint.as_deref().unwrap_or("—")} " · Crash restarts: {runtime.crash_restarts}" }
                if let Some(owner) = &runtime.shared_profile_owner { p { "Shared profile owner: {owner}" } }
                if let Some(error) = &runtime.last_error { p { role: "status", "{error}" } }
                if supports(&provider, None, ProviderCapability::ConfigureProxy) {
                    ProxyConfig { provider: provider.clone(), runtime: runtime.clone(), revision }
                }
            } else { p { class: "muted", "Proxy runtime not observed." } }
            div { class: "row",
                for (action, label) in [(ProxyAction::Start, "Start"), (ProxyAction::Stop, "Stop"), (ProxyAction::Restart, "Restart")] {
                    OperationButton { key: "{label}", provider: provider.clone(), label, confirm: action != ProxyAction::Start,
                        disabled: view.runtime.as_ref().is_some_and(|r| match action { ProxyAction::Start => r.running, ProxyAction::Stop | ProxyAction::Restart => !r.running }),
                        command: ProviderCommand::Routing(RoutingCommand::ControlProxy { action }) }
                }
                OperationButton { provider: provider.clone(), label: "Regenerate Client Key", confirm: true,
                    disabled: view.runtime.as_ref().is_some_and(|r| r.shared_profile_owner.is_some()),
                    command: ProviderCommand::Routing(RoutingCommand::RegenerateClientKey) }
            }
        }
        RoutingPolicyView { provider: provider.clone(), policy: view.policy, revision }
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
    let port_num = port().parse::<u16>().ok().filter(|n| *n > 0);
    rsx! { div { class: "row",
        Field { label: "Port", kind: "number", value: port(), change: move |v| port.set(v) }
        Check { label: "Bind on LAN", value: lan(), change: move |v| lan.set(v) }
        Check { label: "Auto start", value: auto(), change: move |v| auto.set(v) }
        OperationButton { provider, label: "Save proxy configuration", revision, confirm: lan() && !runtime.lan_bind,
            disabled: !port().is_empty() && port_num.is_none(),
            command: ProviderCommand::Routing(RoutingCommand::UpdateProxyConfig {
                port: port_num, lan_bind: Some(lan()), auto_start: Some(auto()), debug_logs: None,
            }) }
    } }
}

const STRATEGIES: &[(RouteStrategy, &str)] = &[
    (RouteStrategy::SoonestReset, "Soonest Reset"),
    (RouteStrategy::FillFirst, "Fill First"),
    (RouteStrategy::RoundRobin, "Round Robin"),
    (RouteStrategy::WeightedRoundRobin, "Weighted Round Robin"),
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
    let mask = use_context::<PanelState>().settings.read().mask_emails;
    let mut options = vec![(String::new(), "Automatic".into())];
    options.extend(
        provider.accounts.iter().filter(|a| a.enabled || preferred.read().as_str() == a.id).map(|a| (a.id.clone(), account_name(a, mask))),
    );
    rsx! { div { class: "provider-inset stack",
        h3 { "Routing policy" }
        p { class: "muted", "Revision {revision} · current strategy: {route_strategy_label(current.strategy)} · key order: {api_key_order_label(current.api_key_order)}" }
        if supports(&provider, None, ProviderCapability::SetRoutePolicy) {
            div { class: "row",
                Choice { label: "Strategy", value: format!("{:?}", current.strategy),
                    options: STRATEGIES.iter().map(|(v, label)| (format!("{v:?}"), label.to_string())).collect(),
                    change: move |key: String| if let Some((v, _)) = STRATEGIES.iter().find(|(v, _)| format!("{v:?}") == key) { draft.write().strategy = *v; } }
                Choice { label: "API key order", value: format!("{:?}", current.api_key_order),
                    options: ORDERS.iter().map(|(v, label)| (format!("{v:?}"), label.to_string())).collect(),
                    change: move |key: String| if let Some((v, _)) = ORDERS.iter().find(|(v, _)| format!("{v:?}") == key) { draft.write().api_key_order = *v; } }
                Field { label: "Session affinity (s)", kind: "number", value: affinity(), change: move |v| affinity.set(v) }
                Field { label: "Failover cooldown (s)", kind: "number", value: cooldown(), change: move |v| cooldown.set(v) }
                Field { label: "Near-reset wait (s)", kind: "number", value: near_reset(), change: move |v| near_reset.set(v) }
                Field { label: "First-byte timeout (s)", kind: "number", value: first_byte(), change: move |v| first_byte.set(v) }
                Check { label: "Switch on server error", value: current.switch_on_server_error, change: move |v| draft.write().switch_on_server_error = v }
                OperationButton { provider: provider.clone(), label: "Save policy", revision, disabled: !valid,
                    command: ProviderCommand::Routing(RoutingCommand::SetRoutePolicy { policy }) }
            }
        }
        if supports(&provider, None, ProviderCapability::SetPoolOverride) {
            div { class: "row",
                Choice { label: "Preferred pool account", value: preferred(), options, change: move |v| preferred.set(v) }
                OperationButton { provider, label: "Apply override", revision, confirm: true,
                    command: ProviderCommand::Routing(RoutingCommand::SetPoolOverride { account_id: optional(&preferred()) }) }
            }
        }
    } }
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
