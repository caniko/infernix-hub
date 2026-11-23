//! Today: the operator's queue from `GET /v1/today`.

use contracts::{Today, TodayItem};
use dioxus::prelude::*;

use crate::{
    actions,
    components::{Chip, EmptyState, ErrorCard, Loading},
    format, labels,
    state::{Store, Workspace},
};

fn buckets(today: &Today) -> [(&'static str, &'static str, &[TodayItem]); 7] {
    [
        ("Pending reviews", "severity-warn", &today.pending_reviews),
        ("Active production", "severity-info", &today.active_production),
        ("Due releases", "severity-info", &today.due_releases),
        ("Failed operations", "severity-crit", &today.failed_operations),
        ("Overdue interactions", "severity-warn", &today.overdue_interactions),
        ("Opportunity deadlines", "severity-warn", &today.opportunities),
        ("Renewals due", "severity-warn", &today.renewals),
    ]
}

fn row_status(subject_type: &str, status: &str) -> String {
    match (subject_type, status) {
        ("renewal", "unknown") => "No due date set".to_string(),
        ("renewal", "due") => "Due soon".to_string(),
        _ => status.replace('_', " "),
    }
}

fn row_action(subject_type: &str) -> Option<&'static str> {
    match subject_type {
        "relationship" => Some("Open conversation ›"),
        "renewal" => Some("Open in Notifications ›"),
        "opportunity" => None,
        _ => Some("Open"),
    }
}

#[component]
pub fn TodayView() -> Element {
    let store = use_context::<Store>();
    let mut bucket = use_signal(|| Option::<&'static str>::None);
    let today = (store.today).cloned();
    let selected = bucket.cloned();

    rsx! {
        aside { class: "context", aria_label: "Today queues",
            div { class: "context-head",
                h2 { "Queues" }
            }
            div { class: "context-list",
                if let Some(data) = today.data.as_ref() {
                    button {
                        class: if selected.is_none() { "context-item active" } else { "context-item" },
                        aria_pressed: selected.is_none(),
                        onclick: move |_| bucket.set(None),
                        span { class: "ctx-title", "All queues" }
                    }
                    for (label, _, items) in buckets(data) {
                        button {
                            key: "{label}",
                            class: if selected == Some(label) { "context-item active" } else { "context-item" },
                            style: "flex-direction: row; align-items: center; justify-content: space-between; gap: 8px;",
                            aria_pressed: selected == Some(label),
                            onclick: move |_| bucket.set(Some(label)),
                            span { class: "ctx-title", "{label}" }
                            if items.is_empty() {
                                span { class: "form-help", style: "padding-inline: 6px;", "0" }
                            } else {
                                span { class: "badge-count", "{items.len()}" }
                            }
                        }
                    }
                } else {
                    p { class: "ctx-empty", "Loading queues…" }
                }
            }
        }
        main { class: "work",
            div { class: "content-bounded",
                div { class: "work-head",
                    h1 { "Needs attention" }
                    p { class: "hint", "Your review, production, and follow-up queues." }
                }
                if let Some(error) = today.error.clone() {
                    ErrorCard { code: error.code, message: error.message }
                }
                match today.data.as_ref() {
                    Some(data) => {
                        let mut shown: Vec<_> = buckets(data)
                            .into_iter()
                            .filter(|(label, _, items)| selected.map_or(!items.is_empty(), |name| name == *label))
                            .collect();
                        shown.sort_by_key(|(_, tone, _)| match *tone {
                            "severity-crit" => 0,
                            "severity-warn" => 1,
                            _ => 2,
                        });
                        rsx! {
                            for (label, tone, items) in shown.iter().copied() {
                                section { class: "panel", key: "{label}",
                                    div { class: "row",
                                        if selected.is_some() {
                                            button { class: "btn ghost btn-xs", onclick: move |_| bucket.set(None), "‹ All queues" }
                                        }
                                        h2 { style: "font-size: 15px;", "{label}" }
                                        if !items.is_empty() {
                                            span { class: "badge-count", "{items.len()}" }
                                        }
                                    }
                                    if items.is_empty() {
                                        EmptyState { title: format!("No {}.", label.to_lowercase()), hint: None }
                                    }
                                    for (index, item) in items.iter().enumerate() {
                                        TodayRow { key: "{item.subject_id}-{index}", item: item.clone(), severity: tone }
                                    }
                                }
                            }
                            if shown.is_empty() {
                                EmptyState { title: "Nothing needs attention".to_string(), hint: None }
                            }
                        }
                    },
                    None if today.error.is_none() => rsx! { Loading { text: "Loading today…".to_string() } },
                    None => rsx! {},
                }
            }
        }
    }
}

#[component]
fn TodayRow(item: TodayItem, severity: &'static str) -> Element {
    let store = use_context::<Store>();
    let subject_type = item.subject_type.clone();
    let tone = labels::status_tone(&item.status).to_string();
    let kind = item.kind.replace('_', " ");
    rsx! {
        div { class: "severity-row {severity} today-row", style: "overflow-wrap: anywhere;",
            div { class: "row",
                strong { class: "today-title", "{item.title}" }
                Chip { label: row_status(&subject_type, &item.status), tone: Some(tone) }
            }
            if let Some(detail) = item.detail.as_deref().filter(|detail| *detail != item.title) {
                p { class: "form-help", "{detail}" }
            }
            div { class: "row",
                span { class: "form-help", "{kind}" }
                if let Some(at) = item.at {
                    if matches!(item.subject_type.as_str(), "opportunity" | "renewal") || (item.kind == "publication" && severity != "severity-crit") {
                        span { class: "form-help nowrap-unit", "Due {format::fmt_dt(at)}" }
                    } else {
                        span { class: "form-help nowrap-unit", "{format::rel_time(at.timestamp_millis().max(0) as u64)}" }
                    }
                }
                div { class: "spacer" }
                if let Some(label) = row_action(&subject_type) {
                    button {
                        class: "btn btn-glass btn-xs",
                        aria_label: "{label}: {item.title}",
                        onclick: move |_| {
                            if subject_type == "renewal" {
                                store.set_workspace(Workspace::Notifications);
                            } else {
                                actions::open_subject(store, &subject_type, item.subject_id);
                            }
                        },
                        "{label}"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_rows_explain_renewal_status_and_name_their_destination() {
        assert_eq!(row_status("renewal", "unknown"), "No due date set");
        assert_eq!(row_status("renewal", "due"), "Due soon");
        assert_eq!(row_status("renewal", "overdue"), "overdue");
        assert_eq!(row_status("content", "in_review"), "in review");
        assert_eq!(row_action("renewal"), Some("Open in Notifications ›"));
        assert_eq!(row_action("relationship"), Some("Open conversation ›"));
        assert_eq!(row_action("opportunity"), None);
        assert_eq!(row_action("content"), Some("Open"));
    }
}
