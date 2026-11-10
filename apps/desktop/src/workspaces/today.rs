//! Today: the operator's queue from `GET /v1/today`.

use contracts::{Today, TodayItem};
use dioxus::prelude::*;

use crate::{
    actions,
    components::{Chip, EmptyState, ErrorCard, Loading, Panel, StatCard},
    format, labels,
    shell::ContextItem,
    state::Store,
};

fn buckets(today: &Today) -> [(&'static str, &'static str, &Vec<TodayItem>); 5] {
    [
        ("pending_reviews", "Pending reviews", &today.pending_reviews),
        ("active_production", "Active production", &today.active_production),
        ("due_releases", "Due releases", &today.due_releases),
        ("failed_operations", "Failed operations", &today.failed_operations),
        ("overdue_interactions", "Overdue interactions", &today.overdue_interactions),
    ]
}

#[component]
pub fn TodayView() -> Element {
    let store = use_context::<Store>();
    let mut bucket = use_signal(|| Option::<&'static str>::None);
    let today = (store.today).cloned();
    let selected = bucket.cloned();

    rsx! {
        aside { class: "context", aria_label: "Today buckets",
            div { class: "context-head",
                h2 { "Today" }
                span { class: "label-tech", "live" }
            }
            div { class: "context-list",
                if let Some(data) = today.data.as_ref() {
                    for (key, label, items) in buckets(data) {
                        ContextItem {
                            key: "{key}",
                            title: label.to_string(),
                            meta: items.len().to_string(),
                            active: selected == Some(key),
                            on_select: move |_| bucket.set(if selected == Some(key) { None } else { Some(key) }),
                        }
                    }
                } else {
                    p { class: "ctx-empty", "Loading buckets…" }
                }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "Today" }
                    p { class: "hint", "Reviews waiting, production running, releases due, failures and cold threads." }
                }
            }
            if let Some(error) = today.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match today.data.clone() {
                Some(data) => rsx! {
                    div { class: "stat-row",
                        StatCard { label: "Pending reviews".to_string(), value: data.pending_reviews.len().to_string(), tone: Some("warn".to_string()), detail: Some("awaiting operator".to_string()) }
                        StatCard { label: "Active production".to_string(), value: data.active_production.len().to_string(), tone: None, detail: Some("generation in flight".to_string()) }
                        StatCard { label: "Due releases".to_string(), value: data.due_releases.len().to_string(), tone: None, detail: Some("next 24h".to_string()) }
                        StatCard { label: "Failed operations".to_string(), value: data.failed_operations.len().to_string(), tone: Some("bad".to_string()), detail: Some("needs attention".to_string()) }
                        StatCard { label: "Overdue interactions".to_string(), value: data.overdue_interactions.len().to_string(), tone: Some("warn".to_string()), detail: Some("replies owed".to_string()) }
                    }
                    for (key, label, items) in buckets(&data) {
                        if selected.is_none() || selected == Some(key) {
                            Panel {
                                key: "{key}",
                                title: label.to_string(),
                                caption: Some(items.len().to_string()),
                                if items.is_empty() {
                                    EmptyState { title: format!("No {}.", label.to_lowercase()), hint: None }
                                } else {
                                    div { class: "stack",
                                        for item in items.iter() {
                                            TodayRow { key: "{item.subject_type}-{item.subject_id}", item: item.clone() }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if data.pending_reviews.is_empty() && data.active_production.is_empty()
                        && data.due_releases.is_empty() && data.failed_operations.is_empty()
                        && data.overdue_interactions.is_empty()
                    {
                        EmptyState { title: "Nothing needs the operator right now.".to_string(), hint: Some("The poll refreshes this view every 1.5s.".to_string()) }
                    }
                },
                None if today.error.is_none() => rsx! { Loading { text: "Loading today…".to_string() } },
                None => rsx! {},
            }
        }
    }
}

#[component]
fn TodayRow(item: TodayItem) -> Element {
    let store = use_context::<Store>();
    let subject_type = item.subject_type.clone();
    let tone = labels::status_tone(&item.status).to_string();
    let kind = item.kind.replace('_', " ");
    rsx! {
        div { class: "glass-card today-row",
            div { class: "spread",
                span { class: "today-title", "{item.title}" }
                Chip { label: item.status.replace('_', " "), tone: Some(tone) }
            }
            div { class: "row",
                span { class: "label-tech", "{kind} · {item.subject_type}" }
                if let Some(at) = item.at {
                    span { class: "label-tech", "{format::rel_time(at.timestamp_millis().max(0) as u64)}" }
                }
                div { class: "spacer" }
                button {
                    class: "btn btn-glass btn-xs",
                    onclick: move |_| actions::open_subject(store, &subject_type, item.subject_id),
                    "Open"
                }
            }
        }
    }
}
