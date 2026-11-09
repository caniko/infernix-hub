//! App shell: sidebar, topbar, status bar, context-list primitives.
//! Adapted from the donor's `Sidebar`/`NavItem`/`Topbar`/`StatusBar`
//! (main.rs lines ~370-460). The brand is the hub's own; no donor logo.

use dioxus::prelude::*;

use crate::{Workspace, icons};

#[component]
pub fn Sidebar(workspace: Signal<Workspace>, nav_open: Signal<bool>) -> Element {
    rsx! {
        nav {
            class: if nav_open.cloned() { "sidebar open" } else { "sidebar" },
            aria_label: "Workspaces",
            div { class: "brand-lockup",
                span { class: "brand-mark", aria_hidden: "true" }
                div { class: "wordmark",
                    "infernix"
                    span { class: "wordmark-sub", "hub console" }
                }
            }
            for (i, w) in Workspace::ALL.iter().enumerate() {
                NavItem { key: "{w.label()}", workspace, nav_open, target: *w, index: i }
            }
            div { class: "sidebar-foot",
                p { class: "label-tech", "Ctrl+K · commands" }
                p { class: "label-tech", "1–6 · workspaces" }
            }
        }
    }
}

#[component]
fn NavItem(
    workspace: Signal<Workspace>,
    nav_open: Signal<bool>,
    target: Workspace,
    index: usize,
) -> Element {
    let active = workspace.cloned() == target;
    let mut workspace = workspace;
    let mut nav_open = nav_open;
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
            span { class: "nav-key", "{index + 1}" }
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
pub fn ContextItem(
    title: String,
    meta: String,
    active: bool,
    on_select: EventHandler<()>,
) -> Element {
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
