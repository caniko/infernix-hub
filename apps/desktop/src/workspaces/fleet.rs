//! Fleet workspaces. Everything here is read-only: the hub serves one snapshot
//! written by `canix fleet status --write`, and approvals happen in a terminal.

use contracts::fleet::{FleetStatus, HostStatus, PendingOp, ServiceAlert};
use dioxus::prelude::*;

use crate::{
    components::{Chip, EmptyState, ErrorCard, Loading, Panel},
    fleet, format, labels,
    state::{Loaded, Store, Workspace},
};

/// True when the hub has no snapshot file at all; all four views say so once.
fn unconfigured(loaded: &Loaded<FleetStatus>) -> bool {
    loaded.error.as_ref().is_some_and(|error| error.code == "fleet_status_unconfigured")
}

fn ago(value: &str) -> String {
    fleet::parse_ms(value).map(format::rel_time).unwrap_or_else(|| value.to_string())
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
    let now = format::now_ms();
    rsx! {
        main { class: "work",
            div { class: "work-head",
                h1 { "Command Center" }
                p { class: "hint", "Fleet health first; content status below." }
            }
            if let Some(error) = loaded.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match loaded.data.clone() {
                Some(status) => rsx! { CommandBody { status, now } },
                None if loaded.error.is_none() => rsx! { Loading { text: "Loading fleet status…".to_string() } },
                None if unconfigured(&loaded) => rsx! { Unconfigured {} },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn CommandBody(status: FleetStatus, now: u64) -> Element {
    let store = use_context::<Store>();
    let counts = fleet::counts(&status);
    let rows = fleet::attention_rows(&status);
    let stale = fleet::stale(now, &status);
    let age_label = fleet::parse_ms(&status.generated_at).map(format::rel_time).unwrap_or_else(|| "unknown".to_string());
    let today = (store.today).cloned().data;

    rsx! {
        div { class: "stat-row",
            FleetTile {
                label: "HOSTS".to_string(),
                value: fleet::hosts_ratio(counts),
                detail: Some("healthy / total".to_string()),
                tone: Some(if counts.healthy < counts.hosts { "warn" } else { "ok" }.to_string()),
                on_activate: move |_| store.set_workspace(Workspace::Hosts),
            }
            FleetTile {
                label: "OUTDATED".to_string(),
                value: counts.outdated.to_string(),
                detail: Some("older revision".to_string()),
                tone: Some(if counts.outdated > 0 { "warn" } else { "ok" }.to_string()),
                on_activate: move |_| store.set_workspace(Workspace::Hosts),
            }
            FleetTile {
                label: "ALERTS".to_string(),
                value: counts.alerts.to_string(),
                detail: Some("services down".to_string()),
                tone: Some(if counts.alerts > 0 { "bad" } else { "ok" }.to_string()),
                on_activate: move |_| store.set_workspace(Workspace::Alerts),
            }
            FleetTile {
                label: "APPROVALS".to_string(),
                value: counts.approvals.to_string(),
                detail: Some("waiting for you".to_string()),
                tone: Some(if counts.approvals > 0 { "warn" } else { "ok" }.to_string()),
                on_activate: move |_| store.set_workspace(Workspace::Approvals),
            }
        }
        div { class: "fleet-split",
            Panel { title: "Needs attention".to_string(), caption: Some(rows.len().to_string()),
                if rows.is_empty() {
                    EmptyState { title: "Nothing needs you.".to_string(), hint: None }
                } else {
                    div { class: "stack",
                        for (index, row) in rows.iter().enumerate() {
                            AttentionRow { key: "{index}-{row.title}", row: row.clone() }
                        }
                    }
                }
            }
            Panel { title: "System".to_string(), caption: None,
                div { class: "glass-card system-card",
                    div { class: "kv",
                        span { class: "label-tech", "Snapshot" }
                        span { class: "row",
                            span { class: "v", "{age_label}" }
                            if stale {
                                Chip { label: "Stale".to_string(), tone: Some("warn".to_string()) }
                            }
                        }
                    }
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
                            span { class: "v", "none recorded" }
                        }
                    }
                    div { class: "kv",
                        span { class: "label-tech", "Monitoring" }
                        if let Some(error) = status.monitoring.error.clone() {
                            span { class: "v bad", title: "{error}", "unreadable" }
                        } else {
                            span { class: "v", "{status.monitoring.monitored} monitored" }
                        }
                    }
                }
            }
        }
        Panel { title: "Studio".to_string(), caption: Some("content".to_string()),
            div { class: "stat-row",
                FleetTile {
                    label: "PENDING REVIEWS".to_string(),
                    value: today.as_ref().map(|today| today.pending_reviews.len().to_string()).unwrap_or_else(|| "—".to_string()),
                    detail: Some("awaiting operator".to_string()),
                    tone: Some("warn".to_string()),
                    on_activate: move |_| store.set_workspace(Workspace::Studio),
                }
                FleetTile {
                    label: "DUE RELEASES".to_string(),
                    value: today.as_ref().map(|today| today.due_releases.len().to_string()).unwrap_or_else(|| "—".to_string()),
                    detail: Some("next 24h".to_string()),
                    tone: None,
                    on_activate: move |_| store.set_workspace(Workspace::Studio),
                }
                FleetTile {
                    label: "FAILED WORK".to_string(),
                    value: today.as_ref().map(|today| today.failed_operations.len().to_string()).unwrap_or_else(|| "—".to_string()),
                    detail: Some("generation or publish".to_string()),
                    tone: Some("bad".to_string()),
                    on_activate: move |_| store.set_workspace(Workspace::Studio),
                }
            }
        }
    }
}

#[component]
fn FleetTile(label: String, value: String, detail: Option<String>, tone: Option<String>, on_activate: EventHandler<()>) -> Element {
    rsx! {
        button { class: "glass-card stat-card tile-btn", onclick: move |_| on_activate.call(()),
            span { class: "label-tech", "{label}" }
            span { class: if let Some(tone) = tone { "value {tone}" } else { "value" }, "{value}" }
            if let Some(detail) = detail {
                span { class: "label-tech sub", "{detail}" }
            }
        }
    }
}

#[component]
fn AttentionRow(row: fleet::Attention) -> Element {
    let store = use_context::<Store>();
    let target = row.workspace;
    rsx! {
        button { class: "attention-row", onclick: move |_| store.set_workspace(target),
            span { class: "attention-title", "{row.title}" }
            span { class: "attention-detail", "{row.detail}" }
            span { class: "attention-go label-tech", "{target.label()}" }
        }
    }
}

#[component]
pub fn HostsView() -> Element {
    let store = use_context::<Store>();
    let loaded = (store.fleet).cloned();
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
                    div { class: "host-list",
                        for host in status.hosts.iter() {
                            HostRow {
                                key: "{host.name}",
                                host: host.clone(),
                                target: status.target_revision.clone(),
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
fn HostRow(host: HostStatus, target: Option<String>) -> Element {
    let store = use_context::<Store>();
    let selected = (store.selected_host).cloned() == Some(host.name.clone());
    let (health_label, health_tone) = labels::health(host.health);
    let (update_label, update_tone) = labels::update_state(host.update_state(target.as_deref()));
    let revision = host.revision.clone().unwrap_or_default();
    let name = host.name.clone();
    rsx! {
        button {
            class: if selected { "host-row active" } else { "host-row" },
            aria_pressed: if selected { "true" } else { "false" },
            onclick: move |_| store.set_selected_host(Some(name.clone())),
            span { class: "host-name", "{host.name}" }
            Chip { label: health_label.to_string(), tone: Some(health_tone.to_string()) }
            Chip { label: update_label.to_string(), tone: Some(update_tone.to_string()) }
            span { class: "mono host-rev", title: "{revision}",
                if revision.is_empty() { "—" } else { "{format::short_hash(&revision)}" }
            }
            span { class: "label-tech host-checked", "{ago(&host.checked_at)}" }
        }
    }
}

#[component]
pub fn AlertsView() -> Element {
    let store = use_context::<Store>();
    let loaded = (store.fleet).cloned();
    rsx! {
        main { class: "work",
            div { class: "work-head",
                h1 { "Alerts" }
                p { class: "hint", "Monitored services that are down, grouped as the monitor groups them." }
            }
            if let Some(error) = loaded.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match loaded.data.clone() {
                Some(status) => rsx! { AlertsBody { status } },
                None if loaded.error.is_none() => rsx! { Loading { text: "Loading alerts…".to_string() } },
                None if unconfigured(&loaded) => rsx! { Unconfigured {} },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn AlertsBody(status: FleetStatus) -> Element {
    let groups = fleet::alert_groups(&status.monitoring.alerts);
    rsx! {
        if let Some(error) = status.monitoring.error.clone() {
            div { class: "monitoring-error", role: "alert",
                strong { "Monitoring could not be read" }
                span { ": {error} — the list below is not evidence that services are up." }
            }
        }
        if groups.is_empty() {
            EmptyState {
                title: "No service alerts.".to_string(),
                hint: Some(format!("{} endpoints monitored.", status.monitoring.monitored)),
            }
        } else {
            for (group, alerts) in groups.iter() {
                Panel {
                    key: "group-{group}",
                    title: if group.is_empty() { "Ungrouped".to_string() } else { group.clone() },
                    caption: Some(alerts.len().to_string()),
                    div { class: "stack",
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
    rsx! {
        div { class: "glass-card alert-row",
            div { class: "spread",
                span { class: "alert-name", "{alert.name}" }
                if let Some(since) = alert.since.clone() {
                    span { class: "label-tech", "since {ago(&since)}" }
                }
            }
            for line in alert.detail.iter() {
                p { class: "alert-detail", "{line}" }
            }
        }
    }
}

#[component]
pub fn ApprovalsView() -> Element {
    let store = use_context::<Store>();
    let loaded = (store.fleet).cloned();
    rsx! {
        main { class: "work",
            div { class: "work-head",
                h1 { "Approvals" }
                p { class: "hint", "Privileged operations waiting for you, oldest first." }
            }
            if let Some(error) = loaded.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match loaded.data.clone() {
                Some(status) => rsx! { ApprovalsBody { status } },
                None if loaded.error.is_none() => rsx! { Loading { text: "Loading approvals…".to_string() } },
                None if unconfigured(&loaded) => rsx! { Unconfigured {} },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn ApprovalsBody(status: FleetStatus) -> Element {
    let ops = fleet::oldest_first(&status.pending_ops);
    rsx! {
        p { class: "read-only-note",
            "The hub is read-only here: approve in a terminal with "
            code { "canix host rops" }
            "."
        }
        if ops.is_empty() {
            EmptyState { title: "No approvals waiting.".to_string(), hint: None }
        } else {
            div { class: "stack",
                for op in ops.iter() {
                    ApprovalRow { key: "{op.id}", op: op.clone() }
                }
            }
        }
    }
}

#[component]
fn ApprovalRow(op: PendingOp) -> Element {
    rsx! {
        div { class: "glass-card approval-row",
            div { class: "spread",
                span { class: "approval-title", "{op.title}" }
                span { class: "label-tech", "{ago(&op.created)}" }
            }
            div { class: "row",
                Chip { label: op.host.clone(), tone: Some("info".to_string()) }
                if !op.requested_by.is_empty() {
                    span { class: "label-tech", "requested by {op.requested_by}" }
                }
            }
            if !op.reason.is_empty() {
                p { class: "reason", "{op.reason}" }
            }
        }
    }
}
