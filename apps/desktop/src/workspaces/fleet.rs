//! Fleet workspaces. Everything here is read-only: the hub serves one snapshot
//! written by `canix fleet status --write`, and approvals happen in a terminal.

use contracts::fleet::{FleetStatus, Health, HostStatus, NightlyOutcome, PendingOp, ServiceAlert};
use dioxus::prelude::*;

use crate::{
    components::{Chip, EmptyState, ErrorCard, Loading, Panel, StatCard},
    fleet, format, labels,
    state::{Loaded, Store, Workspace},
};

// These workspaces have no Store selection or inspector. Keep their exact row
// identity across workspace remounts, separate from host inspector selection.
static FLEET_SELECTION: GlobalSignal<Option<(Workspace, String)>> = Signal::global(|| None);

/// True when the hub has no snapshot file at all; all four views say so once.
fn unconfigured(loaded: &Loaded<FleetStatus>) -> bool {
    loaded.error.as_ref().is_some_and(|error| error.code == "fleet_status_unconfigured")
}

fn ago(value: &str) -> String {
    fleet::parse_ms(value).map(format::rel_time).unwrap_or_else(|| value.to_string())
}

fn attention_summary(status: &FleetStatus, counts: fleet::Counts) -> (usize, String) {
    let down = status.monitoring.alerts.len();
    let degraded = status.hosts.iter().filter(|host| host.health == Health::Degraded).count();
    let unreachable = status.hosts.iter().filter(|host| host.health == Health::Unreachable).count();
    let nightly_failed = status.nightly.as_ref().is_some_and(|nightly| nightly.failed());
    let breakdown = [
        (down, "down"),
        (degraded, "degraded"),
        (unreachable, "unreachable"),
        (counts.approvals, if counts.approvals == 1 { "pending approval" } else { "pending approvals" }),
        (counts.alerts - down, "monitoring error"),
        (usize::from(nightly_failed), "failed nightly"),
    ];
    let total = breakdown.iter().map(|(count, _)| count).sum();
    let detail = breakdown.into_iter().filter(|(count, _)| *count > 0).map(|(count, label)| format!("{count} {label}")).collect::<Vec<_>>();
    (total, if detail.is_empty() { "No issues".to_string() } else { detail.join(" · ") })
}

fn down_duration(now: u64, since: &str) -> Option<String> {
    let age = format::rel_time_at(now, fleet::parse_ms(since)?);
    Some(age.strip_suffix(" ago").unwrap_or(&age).to_string())
}

fn failed_check(line: &str) -> String {
    if let Some(code) = line.strip_prefix("[STATUS] == ").filter(|code| code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit())) {
        format!("Expected HTTP status {code}.")
    } else {
        line.to_string()
    }
}

fn reveal_selection(selected: bool, element: Event<MountedData>) {
    if selected {
        spawn(async move {
            let _ = element.scroll_to(ScrollBehavior::Instant).await;
        });
    }
}

/// True when the data on screen is not current: the last refresh failed (the
/// cached snapshot is still shown) or the snapshot itself is old.
fn not_current(loaded: &Loaded<FleetStatus>) -> bool {
    loaded.error.is_some() || loaded.data.as_ref().is_some_and(|status| fleet::stale(format::now_ms(), status))
}

#[component]
fn NotCurrent() -> Element {
    rsx! {
        div { class: "monitoring-error", role: "alert",
            strong { "This snapshot is not current" }
            span { " — everything below is the last known state, not evidence of health." }
        }
    }
}

#[component]
fn Unconfigured() -> Element {
    rsx! {
        EmptyState {
            title: "This hub has no fleet status file configured.".to_string(),
            hint: Some("Point HUB_FLEET_STATUS_FILE (services.fleetix-hub.fleetStatusFile) at the snapshot `canix fleet status --write` produces.".to_string()),
        }
    }
}

#[component]
pub fn CommandView() -> Element {
    let store = use_context::<Store>();
    let loaded = (store.fleet).cloned();
    let stale = not_current(&loaded);
    rsx! {
        main { class: "work",
            div { class: "work-head",
                h1 { "Command Center" }
                p { class: "hint", "Triage fleet issues and check what’s waiting in Studio." }
            }
            if let Some(error) = loaded.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match loaded.data.clone() {
                Some(status) => rsx! {
                    if stale {
                        NotCurrent {}
                    }
                    CommandBody { status, stale }
                },
                None if loaded.error.is_none() => rsx! { Loading { text: "Loading fleet status…".to_string() } },
                None if unconfigured(&loaded) => rsx! { Unconfigured {} },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn CommandBody(status: FleetStatus, stale: bool) -> Element {
    let store = use_context::<Store>();
    let counts = fleet::counts(&status);
    let rows = fleet::attention_rows(&status);
    let nightly_attention = status.nightly.as_ref().filter(|nightly| nightly.failed()).and_then(|nightly| {
        let mut failed = nightly.hosts.iter().filter(|host| host.outcome == NightlyOutcome::Failed);
        let host =
            failed.next().filter(|_| failed.next().is_none()).and_then(|failed| status.hosts.iter().find(|host| host.name == failed.host));
        // The snapshot helper appends the aggregate nightly issue last.
        rows.last().map(|row| (row, host))
    });
    // Old numbers are shown, but never in the colour of health.
    let ok = if stale { "" } else { "ok" };
    let age_label =
        fleet::snapshot_age_ms(format::now_ms(), &status).map_or_else(|| "Last update unknown".to_string(), |_| ago(&status.generated_at));
    let down = status.monitoring.alerts.len();
    let (attention_total, attention_detail) = attention_summary(&status, counts);
    let today = (store.today).cloned().data;
    let (studio_status, studio_tone) = match &today {
        Some(today) if !today.failed_operations.is_empty() => ("Work failed", Some("bad".to_string())),
        Some(today) if !today.pending_reviews.is_empty() => ("Review needed", Some("warn".to_string())),
        Some(today) if !today.due_releases.is_empty() => ("Releases due", Some("warn".to_string())),
        Some(_) => ("Clear", Some("ok".to_string())),
        None => ("Unavailable", None),
    };
    let studio_detail = today.as_ref().map(|today| {
        format!("{} reviews · {} due · {} failed", today.pending_reviews.len(), today.due_releases.len(), today.failed_operations.len(),)
    });

    rsx! {
        div { class: "panel", style: "margin-bottom: 12px;",
            div { class: "compact-kpi-grid",
                StatCard {
                    label: "Needs attention".to_string(),
                    value: attention_total.to_string(),
                    detail: Some(attention_detail),
                    tone: Some(if down > 0 { "bad" } else if attention_total > 0 { "warn" } else { ok }.to_string()),
                }
                StatCard {
                    label: "System freshness".to_string(),
                    value: if stale { "Stale".to_string() } else { "Current".to_string() },
                    detail: Some(age_label),
                    tone: Some(if stale { "warn" } else { ok }.to_string()),
                }
                FleetTile {
                    label: "Hosts".to_string(),
                    value: fleet::hosts_ratio(counts),
                    detail: Some("Healthy / total".to_string()),
                    // Daytime switches can be newer than the nightly's revision.
                    secondary_detail: Some(format!("{} on another revision", counts.outdated)),
                    tone: Some(if counts.healthy < counts.hosts { "warn" } else { ok }.to_string()),
                    on_activate: move |_| store.set_workspace(Workspace::Hosts),
                }
                FleetTile {
                    label: "Studio".to_string(),
                    value: studio_status.to_string(),
                    detail: studio_detail,
                    tone: studio_tone,
                    on_activate: move |_| store.set_workspace(Workspace::Studio),
                }
            }
        }
        div { class: "fleet-split", style: "gap: 4px 16px;",
            Panel { title: "Needs attention".to_string(), caption: None,
                if attention_total == 0 {
                    EmptyState {
                        title: (if stale { "No known issues in the last snapshot." } else { "Nothing needs attention right now." }).to_string(),
                        hint: None,
                    }
                } else {
                    div { class: "stack", style: "gap: 4px;",
                        for alert in &status.monitoring.alerts {
                            AttentionRow {
                                key: "alert-{alert.key}",
                                title: format!("{} is down", alert.name),
                                detail: alert.detail.first().map_or_else(|| "The service failed its health check.".into(), |line| failed_check(line)),
                                workspace: Workspace::Alerts,
                                item: Some(alert.key.clone()),
                                severity: "severity-crit",
                            }
                        }
                        if let Some(error) = &status.monitoring.error {
                            AttentionRow {
                                title: "Monitoring could not be read".to_string(),
                                detail: error.clone(),
                                workspace: Workspace::Alerts,
                                item: None,
                                severity: "severity-warn",
                            }
                        }
                        for host in status.hosts.iter().filter(|host| host.health != Health::Healthy) {
                            AttentionRow {
                                key: "host-{host.name}",
                                title: format!("{}: {}", host.name, labels::health(host.health).0),
                                detail: host.failures.first().cloned().unwrap_or_else(|| "Check the host for details.".into()),
                                workspace: Workspace::Hosts,
                                item: Some(host.name.clone()),
                                severity: "severity-warn",
                            }
                        }
                        for op in &status.pending_ops {
                            AttentionRow {
                                key: "op-{op.id}",
                                title: op.title.clone(),
                                detail: format!("Waiting for approval on {}.", op.host),
                                workspace: Workspace::Approvals,
                                item: Some(op.id.clone()),
                                severity: "severity-info",
                            }
                        }
                        if let Some((row, host)) = nightly_attention {
                            AttentionRow {
                                title: row.title.clone(),
                                detail: row.detail.clone(),
                                workspace: row.workspace,
                                item: host.map(|host| host.name.clone()),
                                severity: "severity-warn",
                            }
                        }
                    }
                }
            }
            Panel { title: "System".to_string(), caption: None,
                div { class: "glass-card system-card",
                    div { class: "kv",
                        span { class: "label-tech", "Target revision" }
                        if let Some(target) = status.target_revision.clone() {
                            span { class: "v", title: "{target}", "{format::short_hash(&target)}" }
                        } else {
                            span { class: "v", "—" }
                        }
                    }
                    if let Some(nightly) = status.nightly.clone() {
                        div { class: "kv",
                            span { class: "label-tech", "Last nightly" }
                            span { class: "v", "{ago(&nightly.started_at)}" }
                        }
                        if let Some(commit) = nightly.commit.clone() {
                            div { class: "kv",
                                span { class: "label-tech", "Commit" }
                                span { class: "v", title: "{commit}", "{format::short_hash(&commit)}" }
                            }
                        }
                        if let Some(error) = nightly.error.clone() {
                            div { class: "kv",
                                span { class: "label-tech", "Nightly error" }
                                span { class: "v", "{error}" }
                            }
                        }
                        for host in nightly.hosts.iter() {
                            div { class: "kv", key: "nightly-{host.host}",
                                span { class: "label-tech", "{host.host}" }
                                span { class: "row",
                                    {
                                        let (label, tone) = labels::nightly_outcome(host.outcome);
                                        rsx! { Chip { label: label.to_string(), tone: Some(tone.to_string()) } }
                                    }
                                    if let Some(detail) = host.detail.clone() {
                                        span { class: "v", title: "{detail}", "{format::preview(&detail, 40)}" }
                                    }
                                }
                            }
                        }
                    } else {
                        div { class: "kv",
                            span { class: "label-tech", "Last nightly" }
                            small { class: "muted", "none recorded" }
                        }
                    }
                    if status.monitoring.error.is_none() {
                        div { class: "kv",
                            span { class: "label-tech", "Monitoring" }
                            span { class: "v", "{status.monitoring.monitored} monitored" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn FleetTile(
    label: String,
    value: String,
    detail: Option<String>,
    secondary_detail: Option<String>,
    tone: Option<String>,
    on_activate: EventHandler<()>,
) -> Element {
    rsx! {
        button { class: "glass-card stat-card tile-btn", onclick: move |_| on_activate.call(()),
            span { class: "label-tech", "{label}" }
            span { class: if let Some(tone) = tone { "value {tone}" } else { "value" }, "{value}" }
            if let Some(detail) = detail {
                span { class: "label-tech sub", "{detail}" }
            }
            if let Some(detail) = secondary_detail {
                span { class: "label-tech sub", "{detail}" }
            }
        }
    }
}

fn attention_action(workspace: Workspace, has_item: bool) -> &'static str {
    match (workspace, has_item) {
        (Workspace::Alerts, true) => "Inspect alert",
        (Workspace::Alerts, false) => "Review alerts",
        (Workspace::Hosts, true) => "Inspect host",
        (Workspace::Hosts, false) => "Review hosts",
        (Workspace::Approvals, _) => "Review approval",
        _ => workspace.label(),
    }
}

#[component]
fn AttentionRow(title: String, detail: String, workspace: Workspace, item: Option<String>, severity: &'static str) -> Element {
    let store = use_context::<Store>();
    let action = attention_action(workspace, item.is_some());
    rsx! {
        button { class: "list-table-row severity-row {severity}",
            style: "padding-block: 6px;",
            onclick: move |_| {
                match workspace {
                    Workspace::Hosts => store.set_selected_host(item.clone()),
                    Workspace::Alerts | Workspace::Approvals => *FLEET_SELECTION.write() = item.clone().map(|id| (workspace, id)),
                    _ => {}
                }
                store.set_workspace(workspace);
            },
            span { style: "display: grid;",
                strong { "{title}" }
                span { class: "muted", style: "font-size: 12px;", "{detail}" }
            }
            span { class: "row-action-pill nowrap-unit",
                "{action} "
                span { aria_hidden: "true", "›" }
            }
        }
    }
}

#[component]
pub fn HostsView() -> Element {
    let store = use_context::<Store>();
    let loaded = (store.fleet).cloned();
    let stale = not_current(&loaded);
    rsx! {
        main { class: "work",
            div { class: "work-head",
                h1 { "Hosts" }
                p { class: "hint", "Health, revision and failed checks. Select a host for the inspector." }
            }
            if let Some(error) = loaded.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match loaded.data.clone() {
                Some(status) if status.hosts.is_empty() => rsx! {
                    EmptyState { title: "The snapshot lists no hosts.".to_string(), hint: None }
                },
                Some(status) => rsx! {
                    if stale {
                        NotCurrent {}
                    }
                    div { class: "list-table hosts-table",
                        style: "--list-columns: minmax(5rem, 1fr) minmax(5.5rem, .8fr) minmax(6rem, 1fr) minmax(3.5rem, .6fr) 6rem;",
                        div { class: "list-table-head",
                            span { "Host" }
                            span { "Health" }
                            span { "Revision" }
                            span { "Checked" }
                            span { aria_hidden: "true", " " }
                        }
                        for host in status.hosts.iter() {
                            HostRow {
                                key: "{host.name}",
                                host: host.clone(),
                                target: status.target_revision.clone(),
                                stale,
                            }
                        }
                    }
                },
                None if loaded.error.is_none() => rsx! { Loading { text: "Loading hosts…".to_string() } },
                None if unconfigured(&loaded) => rsx! { Unconfigured {} },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn HostRow(host: HostStatus, target: Option<String>, stale: bool) -> Element {
    let store = use_context::<Store>();
    let selected = (store.selected_host).read().as_deref() == Some(host.name.as_str());
    let open = selected && (store.inspector_open).cloned();
    let (health_label, health_tone) = match labels::health(host.health) {
        (label, "ok") if stale => (format!("{label} (last known)"), ""),
        (label, tone) => (label.to_string(), tone),
    };
    let (update_label, update_tone) = labels::update_state(host.update_state(target.as_deref()));
    let revision = host.revision.clone().unwrap_or_default();
    let name = host.name.clone();
    rsx! {
        button {
            class: "list-table-row",
            aria_pressed: if selected { "true" } else { "false" },
            onmounted: move |element| reveal_selection(selected, element),
            onclick: move |_| store.set_selected_host(Some(name.clone())),
            strong { "{host.name}" }
            span {
                Chip { label: health_label, tone: Some(health_tone.to_string()) }
            }
            span { class: "row",
                Chip { label: update_label.to_string(), tone: Some(update_tone.to_string()) }
                span { class: "mono host-rev", title: "{revision}",
                    if revision.is_empty() { "—" } else { "{format::short_hash(&revision)}" }
                }
            }
            span { class: "muted", "{ago(&host.checked_at)}" }
            if open {
                span { class: "row-chevron chip amber nowrap-unit", "Inspecting" }
            } else {
                span { class: "row-chevron", aria_hidden: "true", "›" }
            }
        }
    }
}

#[component]
pub fn AlertsView() -> Element {
    let store = use_context::<Store>();
    let loaded = (store.fleet).cloned();
    let stale = not_current(&loaded);
    rsx! {
        main { class: "work",
            div { class: "work-head",
                h1 { "Alerts" }
                p { class: "hint", "Down services by monitor group. Select a service to inspect its failed checks." }
            }
            if let Some(error) = loaded.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match loaded.data.clone() {
                Some(status) => rsx! {
                    if stale {
                        NotCurrent {}
                    }
                    AlertsBody { status, stale }
                },
                None if loaded.error.is_none() => rsx! { Loading { text: "Loading alerts…".to_string() } },
                None if unconfigured(&loaded) => rsx! { Unconfigured {} },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn AlertsBody(status: FleetStatus, stale: bool) -> Element {
    let groups = fleet::alert_groups(&status.monitoring.alerts);
    rsx! {
        if let Some(error) = status.monitoring.error.clone() {
            div { class: "monitoring-error", role: "alert",
                strong { "Monitoring could not be read" }
                span { ": {error}. Service health could not be verified." }
            }
        }
        if groups.is_empty() {
            if status.monitoring.error.is_none() {
                EmptyState {
                    title: (if stale { "No service alerts in the last snapshot." } else { "No service alerts." }).to_string(),
                    hint: Some(format!("{} endpoints monitored.", status.monitoring.monitored)),
                }
            }
        } else {
            for (group, alerts) in groups.iter() {
                section { class: "panel", style: "max-width: 36rem;", key: "group-{group}",
                    div { class: "panel-head",
                        h3 { class: "row",
                            if group.is_empty() { "Ungrouped" } else { "{group}" }
                            span { class: "badge-count", "{alerts.len()}" }
                        }
                    }
                    div { class: "stack", style: "gap: 6px;",
                        for alert in alerts.iter() {
                            AlertRow { key: "{alert.key}", alert: alert.clone() }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn AlertRow(alert: ServiceAlert) -> Element {
    let selected = FLEET_SELECTION.read().as_ref().is_some_and(|(workspace, key)| *workspace == Workspace::Alerts && key == &alert.key);
    let action = attention_action(Workspace::Alerts, true);
    let duration = alert.since.as_deref().and_then(|since| down_duration(format::now_ms(), since));
    let state = duration.map_or_else(|| "Down".to_string(), |duration| format!("Down · {duration}"));
    let key = alert.key.clone();
    rsx! {
        button {
            class: "list-table-row alert-incident-row severity-crit",
            aria_pressed: if selected { "true" } else { "false" },
            onmounted: move |element| reveal_selection(selected, element),
            onclick: move |_| *FLEET_SELECTION.write() = (!selected).then(|| (Workspace::Alerts, key.clone())),
            span { style: "display: grid; gap: 2px;",
                span { class: "row",
                    strong { class: "alert-name", "{alert.name}" }
                    Chip { label: state, tone: Some("bad".to_string()) }
                }
                for line in alert.detail.iter().take(if selected { usize::MAX } else { 1 }) {
                    span { class: "alert-detail", "{failed_check(line)}" }
                }
                if alert.detail.is_empty() {
                    span { class: "alert-detail", "The service failed its health check." }
                }
            }
            span { class: "row-action-pill nowrap-unit",
                if selected {
                    "Selected"
                } else {
                    "{action} "
                    span { aria_hidden: "true", "›" }
                }
            }
        }
    }
}

#[component]
pub fn ApprovalsView() -> Element {
    let store = use_context::<Store>();
    let loaded = (store.fleet).cloned();
    let stale = not_current(&loaded);
    rsx! {
        main { class: "work",
            div { class: "work-head",
                h1 { "Approvals" }
                p { class: "hint", "Read-only queue of privileged operations, oldest first." }
            }
            if let Some(error) = loaded.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match loaded.data.clone() {
                Some(status) => rsx! {
                    if stale {
                        NotCurrent {}
                    }
                    div { class: "content-bounded", style: "width: min(100%, 36rem);",
                        ApprovalsBody { status }
                    }
                },
                None if loaded.error.is_none() => rsx! { Loading { text: "Loading approvals…".to_string() } },
                None if unconfigured(&loaded) => rsx! { Unconfigured {} },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn ApprovalsBody(status: FleetStatus) -> Element {
    const OPS_COMMAND: &str = "canix host ops";
    let ops = fleet::oldest_first(&status.pending_ops);
    let mut copy_status = use_signal(|| "");
    rsx! {
        div { class: "row glass-card", style: "padding: 4px 8px; margin-bottom: 8px;",
            span { class: "muted", style: "font-size: 12px;",
                "Review requests in your terminal with "
                code { "{OPS_COMMAND}" }
                "."
            }
            button {
                class: "btn primary",
                onclick: move |_| {
                    copy_status.set("");
                    spawn(async move {
                        let eval = document::eval("const text = await dioxus.recv(); await navigator.clipboard.writeText(text); return true;");
                        let copied = eval.send(OPS_COMMAND).is_ok() && eval.join::<bool>().await.unwrap_or(false);
                        copy_status.set(if copied { "Copied" } else { "Clipboard unavailable" });
                    });
                },
                "Copy terminal command"
            }
            span { class: "copy-status", role: "status", "{copy_status}" }
        }
        if ops.is_empty() {
            EmptyState { title: "No approvals waiting.".to_string(), hint: None }
        } else {
            ul { class: "hairline-list",
                for op in ops.iter() {
                    ApprovalRow { key: "{op.id}", op: op.clone() }
                }
            }
        }
    }
}

#[component]
fn ApprovalRow(op: PendingOp) -> Element {
    let selected = FLEET_SELECTION.read().as_ref().is_some_and(|(workspace, id)| *workspace == Workspace::Approvals && id == &op.id);
    rsx! {
        li { class: "hairline-row approval-row", style: "overflow-wrap: anywhere;",
            aria_current: selected.then_some("true"),
            onmounted: move |element| reveal_selection(selected, element),
            h3 { class: "approval-title", "{op.title}" }
            div { class: "row", style: "gap: 4px;",
                Chip { label: format!("Host: {}", op.host), tone: None }
                if !op.requested_by.is_empty() {
                    Chip { label: format!("Requested by {}", op.requested_by), tone: None }
                }
                Chip { label: ago(&op.created), tone: None }
                if selected {
                    Chip { label: "Selected request".to_string(), tone: Some("muted".to_string()) }
                }
            }
            if !op.reason.is_empty() {
                p { class: "op-desc", style: "color: var(--text-muted); font-size: 12px;", "{op.reason}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_actions_name_the_available_destination() {
        assert_eq!(attention_action(Workspace::Alerts, true), "Inspect alert");
        assert_eq!(attention_action(Workspace::Hosts, true), "Inspect host");
        assert_eq!(attention_action(Workspace::Approvals, true), "Review approval");
        assert_eq!(attention_action(Workspace::Alerts, false), "Review alerts");
        assert_eq!(attention_action(Workspace::Hosts, false), "Review hosts");
    }

    #[test]
    fn attention_summary_accounts_for_every_row() {
        let status: FleetStatus = serde_json::from_value(serde_json::json!({
            "schemaVersion": 1,
            "generatedAt": "2026-11-23T12:00:00Z",
            "hosts": [
                { "name": "node-a", "health": "healthy", "checkedAt": "2026-11-23T12:00:00Z" },
                { "name": "node-b", "health": "degraded", "checkedAt": "2026-11-23T12:00:00Z" },
                { "name": "node-c", "health": "unreachable", "checkedAt": "2026-11-23T12:00:00Z" }
            ],
            "monitoring": {
                "error": "relay refused",
                "monitored": 2,
                "alerts": [{ "key": "core_forgejo", "name": "forgejo", "group": "core" }]
            },
            "pendingOps": [
                { "id": "op-1", "host": "node-b", "title": "Restart forgejo", "created": "2026-11-23T12:00:00Z" },
                { "id": "op-2", "host": "node-a", "title": "Finish activation", "created": "2026-11-23T12:00:00Z" }
            ],
            "nightly": {
                "startedAt": "2026-11-23T11:00:00Z",
                "revision": "new",
                "error": "rollout failed",
                "hosts": []
            }
        }))
        .unwrap();
        let (total, detail) = attention_summary(&status, fleet::counts(&status));
        assert_eq!(total, fleet::attention_rows(&status).len());
        assert_eq!(detail, "1 down · 1 degraded · 1 unreachable · 2 pending approvals · 1 monitoring error · 1 failed nightly");
    }

    #[test]
    fn down_duration_reads_as_elapsed_time() {
        let since = "2026-11-23T12:00:00Z";
        let now = fleet::parse_ms(since).unwrap();
        assert_eq!(down_duration(now + 25 * 60 * 1000, since).as_deref(), Some("25m"));
        assert_eq!(down_duration(now, since).as_deref(), Some("now"));
        assert_eq!(down_duration(now, "not a time"), None);
    }

    #[test]
    fn failed_http_check_names_the_expected_status() {
        assert_eq!(failed_check("[STATUS] == 200"), "Expected HTTP status 200.");
        assert_eq!(failed_check("TLS certificate expired"), "TLS certificate expired");
    }
}
