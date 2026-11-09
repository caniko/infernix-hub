//! Shared presentation kit, adapted from the donor's StatCard / GatePrompt and
//! the Stitch `.glass-card` / `.chip` / `.panel` vocabulary. The hub palette
//! lives in `theme.css`; these components only carry structure.

use dioxus::prelude::*;

pub mod chat;
pub mod history;
pub mod proposal;

/// A single headline number with a required text label (never colour-only).
#[component]
pub fn StatCard(label: String, value: String, tone: Option<String>, detail: Option<String>) -> Element {
    rsx! {
        div { class: "glass-card stat-card",
            span { class: "label-tech", "{label}" }
            span { class: if let Some(t) = tone { "value {t}" } else { "value" }, "{value}" }
            if let Some(d) = detail {
                span { class: "label-tech sub", "{d}" }
            }
        }
    }
}

/// A titled panel with an optional right-aligned caption.
#[component]
pub fn Panel(title: String, caption: Option<String>, children: Element) -> Element {
    rsx! {
        section { class: "panel",
            div { class: "panel-head",
                h3 { "{title}" }
                if let Some(c) = caption {
                    span { class: "label-tech", "{c}" }
                }
            }
            {children}
        }
    }
}

/// Explicit empty state — every list gets one.
#[component]
pub fn EmptyState(title: String, hint: Option<String>) -> Element {
    rsx! {
        div { class: "empty glass-card",
            p { "{title}" }
            if let Some(h) = hint {
                p { class: "label-tech", "{h}" }
            }
        }
    }
}

/// One status chip. The label is always text; `tone` only changes the colour.
#[component]
pub fn Chip(label: String, tone: Option<String>) -> Element {
    rsx! {
        span { class: if let Some(t) = tone { "chip {t}" } else { "chip" }, "{label}" }
    }
}

/// Approval controls (donor `GatePrompt`): a labelled gate with an approve and a
/// reject action. Both are real `<button>`s, so keyboard and focus work.
#[component]
pub fn GatePrompt(
    label: String,
    approve_label: String,
    reject_label: String,
    on_approve: EventHandler<()>,
    on_reject: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "gateprompt",
            span { class: "gate-label", "⚑ {label}" }
            button {
                class: "btn btn-sage",
                onclick: move |_| on_approve.call(()),
                "{approve_label}"
            }
            button {
                class: "btn btn-danger-ghost",
                onclick: move |_| on_reject.call(()),
                "{reject_label}"
            }
        }
    }
}

/// Inline notice used for command outcomes.
#[component]
pub fn Notice(ok: bool, message: String) -> Element {
    rsx! {
        p { class: if ok { "notice ok" } else { "notice err" }, role: "status", "{message}" }
    }
}

/// Stable error code + message from an `ApiError`/transport failure.
#[component]
pub fn ErrorCard(code: String, message: String) -> Element {
    rsx! {
        div { class: "error-card", role: "alert",
            span { class: "code", "{code}" }
            span { class: "msg", "{message}" }
        }
    }
}

#[component]
pub fn Loading(text: String) -> Element {
    rsx! {
        p { class: "loading", aria_live: "polite", "{text}" }
    }
}
