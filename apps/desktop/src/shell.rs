//! App shell: sidebar, topbar, status bar, context-list primitives.
//! Adapted from the donor's `Sidebar`/`NavItem`/`Topbar`/`StatusBar`
//! (main.rs lines ~370-460). The brand is the hub's own; no donor logo.

use dioxus::prelude::*;

use crate::{Workspace, fleet, icons, state::Store};

pub fn focus_nav_toggle() {
    let _ = document::eval("document.getElementById('nav-toggle')?.focus()");
}

#[component]
pub fn Sidebar(workspace: Signal<Workspace>, mut nav_open: Signal<bool>, narrow: Signal<bool>) -> Element {
    let store = use_context::<Store>();
    let snapshot = (store.fleet).cloned().data;
    let open = (store.notifications).cloned().open.max(0) as usize;
    let badge = move |target: Workspace| -> usize {
        match target {
            Workspace::Notifications => open,
            Workspace::Hosts => snapshot.as_ref().map(fleet::hosts_needing_attention).unwrap_or_default(),
            Workspace::Alerts => snapshot.as_ref().map(fleet::alert_count).unwrap_or_default(),
            Workspace::Approvals => snapshot.as_ref().map(|status| status.pending_ops.len()).unwrap_or_default(),
            _ => 0,
        }
    };
    let groups: [(&str, Vec<Workspace>); 2] = ["FLEET", "STUDIO"].map(|section| {
        let items = Workspace::ALL.iter().copied().filter(|w| w.section() == section).collect();
        (section, items)
    });
    use_effect(move || {
        if narrow.cloned() && nav_open.cloned() {
            let _ = document::eval("document.querySelector('#workspace-nav [aria-current=page]')?.focus()");
        }
    });

    rsx! {
        nav {
            id: "workspace-nav",
            class: if nav_open.cloned() { "sidebar open" } else { "sidebar" },
            aria_label: "Workspaces",
            aria_hidden: (narrow.cloned() && !nav_open.cloned()).then_some("true"),
            div { class: "brand-lockup",
                span { class: "brand-mark", aria_hidden: "true" }
                div { class: "wordmark",
                    "fleetix"
                    span { class: "wordmark-sub", "hub console" }
                }
                button {
                    class: "icon-btn sidebar-close",
                    aria_label: "Close navigation",
                    onclick: move |_| {
                        nav_open.set(false);
                        focus_nav_toggle();
                    },
                    "✕"
                }
            }
            for (section, items) in groups.iter() {
                div { class: "nav-section label-tech", key: "section-{section}", "{section}" }
                for target in items.iter() {
                    NavItem {
                        key: "{target.label()}",
                        workspace,
                        nav_open,
                        target: *target,
                        badge: badge(*target),
                    }
                }
            }
            div { class: "sidebar-foot",
                p { "Ctrl+K · Commands" }
                p { "Keys: 1–9, 0 · N · U · I · P" }
            }
        }
    }
}

#[component]
fn NavItem(workspace: Signal<Workspace>, nav_open: Signal<bool>, target: Workspace, badge: usize) -> Element {
    let active = workspace.cloned() == target;
    let mut workspace = workspace;
    let mut nav_open = nav_open;
    let key = target.key();
    rsx! {
        button {
            class: if active { "nav-item active" } else { "nav-item" },
            aria_current: if active { "page" } else { "false" },
            onclick: move |_| {
                workspace.set(target);
                if *nav_open.peek() {
                    focus_nav_toggle();
                }
                nav_open.set(false);
            },
            span { class: "ico", aria_hidden: "true", dangerous_inner_html: icons::workspace(target) }
            span { class: "nav-label", "{target.label()}" }
            if badge > 0 {
                span { class: "nav-badge", "{badge}" }
            }
            kbd { class: "kbd-cap nav-key", aria_hidden: "true", "{key}" }
        }
    }
}

#[component]
pub fn Topbar(workspace: Workspace, inspector_open: Signal<bool>, nav_open: Signal<bool>, on_palette: EventHandler<()>) -> Element {
    let mut inspector_open = inspector_open;
    let mut nav_open = nav_open;
    let expanded = inspector_open.cloned();
    rsx! {
        header { class: "topbar",
            div { class: "topbar-heading",
                h1 { class: "title", "{workspace.title()}" }
                span { class: "hint", "{workspace.hint()}" }
            }
            button {
                id: "nav-toggle",
                class: "icon-btn nav-toggle",
                aria_label: "Toggle navigation",
                aria_controls: "workspace-nav",
                aria_expanded: if nav_open.cloned() { "true" } else { "false" },
                onclick: move |_| nav_open.toggle(),
                "☰"
            }
            div { class: "spacer" }
            button {
                class: "btn ghost commands-toggle",
                aria_keyshortcuts: "Control+k Meta+k",
                onclick: move |_| on_palette.call(()),
                "Commands"
                kbd { class: "kbd-cap", aria_hidden: "true", "Ctrl+K" }
            }
            if workspace.supports_inspector() {
                button {
                    class: "icon-btn inspector-toggle",
                    aria_expanded: expanded,
                    onclick: move |_| inspector_open.toggle(),
                    if expanded { "Hide inspector" } else { "Inspector" }
                }
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
pub fn StatusBar(
    connection: StatusSeg,
    fleet: StatusSeg,
    production_paused: bool,
    integrations: Vec<StatusSeg>,
    version: String,
) -> Element {
    rsx! {
        footer { class: "statusbar",
            StatusSegment { seg: connection }
            StatusSegment { seg: fleet }
            if production_paused {
                StatusSegment { seg: StatusSeg { label: "Production paused".into(), tone: "warn".into() } }
            }
            details { class: "status-disclosure",
                summary { class: "status-toggle",
                    "Integrations"
                    if !integrations.is_empty() {
                        span { class: "badge-count", "{integrations.len()}" }
                    }
                }
                div { class: "status-tray",
                    if integrations.is_empty() {
                        p { class: "muted", "No integration status reported." }
                    }
                    for seg in integrations {
                        StatusSegment { key: "{seg.label}", seg }
                    }
                    p { class: "status-version", "{version}" }
                }
            }
        }
    }
}

#[component]
fn StatusSegment(seg: StatusSeg) -> Element {
    rsx! {
        div { class: "seg {seg.tone}",
            span { class: "dot", aria_hidden: "true" }
            "{seg.label}"
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

/// Persistent warning when adapters run on fixtures.
#[component]
pub fn FixtureBanner(all_fixture: bool, simulated: Vec<String>) -> Element {
    let detail = if all_fixture {
        "All integrations are simulated.".to_string()
    } else if simulated.is_empty() {
        "Some integrations are simulated; others are live.".to_string()
    } else {
        format!("Simulated: {}. Other integrations are live.", simulated.join(", "))
    };
    rsx! {
        div { class: "fixture-banner", role: "status",
            strong { "Fixture" }
            span { "{detail}" }
        }
    }
}
