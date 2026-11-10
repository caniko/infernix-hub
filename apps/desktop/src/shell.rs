//! App shell: sidebar, topbar, status bar, context-list primitives.
//! Adapted from the donor's `Sidebar`/`NavItem`/`Topbar`/`StatusBar`
//! (main.rs lines ~370-460). The brand is the hub's own; no donor logo.

use dioxus::prelude::*;

use crate::{Workspace, fleet, icons, state::Store};

#[component]
pub fn Sidebar(workspace: Signal<Workspace>, nav_open: Signal<bool>) -> Element {
    let store = use_context::<Store>();
    let snapshot = (store.fleet).cloned().data;
    let badge = move |target: Workspace| -> usize {
        let Some(status) = snapshot.as_ref() else { return 0 };
        match target {
            Workspace::Hosts => fleet::hosts_needing_attention(status),
            Workspace::Alerts => fleet::alert_count(status),
            Workspace::Approvals => status.pending_ops.len(),
            _ => 0,
        }
    };
    let groups: [(&str, Vec<(usize, Workspace)>); 2] = ["FLEET", "STUDIO"].map(|section| {
        let items = Workspace::ALL.iter().copied().enumerate().filter(|(_, w)| w.section() == section).collect();
        (section, items)
    });

    rsx! {
        nav {
            class: if nav_open.cloned() { "sidebar open" } else { "sidebar" },
            aria_label: "Workspaces",
            div { class: "brand-lockup",
                span { class: "brand-mark", aria_hidden: "true" }
                div { class: "wordmark",
                    "fleetix"
                    span { class: "wordmark-sub", "hub console" }
                }
            }
            for (section, items) in groups.iter() {
                div { class: "nav-section label-tech", key: "section-{section}", "{section}" }
                for (index, target) in items.iter() {
                    NavItem {
                        key: "{target.label()}",
                        workspace,
                        nav_open,
                        target: *target,
                        index: *index,
                        badge: badge(*target),
                    }
                }
            }
            div { class: "sidebar-foot",
                p { class: "label-tech", "Ctrl+K · commands" }
                p { class: "label-tech", "1–9, 0 · workspaces" }
            }
        }
    }
}

#[component]
fn NavItem(workspace: Signal<Workspace>, nav_open: Signal<bool>, target: Workspace, index: usize, badge: usize) -> Element {
    let active = workspace.cloned() == target;
    let mut workspace = workspace;
    let mut nav_open = nav_open;
    // The tenth workspace has no single-digit key; `0` follows `9`.
    let key = if index == 9 { "0".to_string() } else { (index + 1).to_string() };
    rsx! {
        button {
            class: if active { "nav-item active" } else { "nav-item" },
            aria_current: if active { "page" } else { "false" },
            onclick: move |_| {
                workspace.set(target);
                nav_open.set(false);
            },
            span { class: "ico", dangerous_inner_html: icons::workspace(target) }
            span { class: "nav-label", "{target.label()}" }
            if badge > 0 {
                span { class: "nav-badge", "{badge}" }
            }
            span { class: "nav-key", "{key}" }
        }
    }
}

#[component]
pub fn Topbar(
    workspace: Workspace,
    inspector_open: Signal<bool>,
    nav_open: Signal<bool>,
    live: bool,
    status_label: String,
    on_palette: EventHandler<()>,
) -> Element {
    let mut inspector_open = inspector_open;
    let mut nav_open = nav_open;
    rsx! {
        header { class: "topbar",
            button {
                class: "icon-btn nav-toggle",
                aria_label: "Toggle navigation",
                onclick: move |_| nav_open.toggle(),
                "☰"
            }
            div { class: "stack", style: "gap: 0",
                h1 { class: "title", "{workspace.title()}" }
                span { class: "hint", "{workspace.hint()}" }
            }
            div { class: "spacer" }
            div { class: if live { "status-dot live" } else { "status-dot" },
                span { class: "beacon", aria_hidden: "true" }
                "{status_label}"
            }
            button {
                class: "btn btn-glass",
                onclick: move |_| on_palette.call(()),
                "Ctrl+K Commands"
            }
            button {
                class: "icon-btn",
                aria_pressed: if inspector_open.cloned() { "true" } else { "false" },
                aria_label: "Toggle inspector",
                onclick: move |_| inspector_open.toggle(),
                "Inspector"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusSeg {
    pub label: String,
    /// "" | "live" | "warn" | "bad"
    pub tone: String,
}

#[component]
pub fn StatusBar(connected: bool, items: Vec<StatusSeg>, right: String) -> Element {
    rsx! {
        footer { class: "statusbar",
            div { class: if connected { "seg live" } else { "seg bad" },
                span { class: "dot", aria_hidden: "true" }
                if connected { "HUB · CONNECTED" } else { "HUB · OFFLINE" }
            }
            for seg in items.iter() {
                div { class: if seg.tone.is_empty() { "seg" } else { "seg {seg.tone}" },
                    span { class: "dot", aria_hidden: "true" }
                    "{seg.label}"
                }
            }
            div { class: "grow" }
            div { class: "seg", "{right}" }
        }
    }
}

/// A context-list row. Always a `<button>`: selection is an action.
#[component]
pub fn ContextItem(title: String, meta: String, active: bool, on_select: EventHandler<()>) -> Element {
    rsx! {
        button {
            class: if active { "context-item active" } else { "context-item" },
            aria_pressed: if active { "true" } else { "false" },
            onclick: move |_| on_select.call(()),
            span { class: "ctx-title", "{title}" }
            if !meta.is_empty() {
                span { class: "ctx-meta", "{meta}" }
            }
        }
    }
}

/// Persistent warning when any adapter runs on fixtures.
#[component]
pub fn FixtureBanner() -> Element {
    rsx! {
        div { class: "fixture-banner", role: "alert",
            strong { "FIXTURE MODE" }
            span { "— integrations are simulated" }
        }
    }
}
