//! Calendar: publication intents grouped by UTC day and account.
//!
//! `/v1/intents` carries delivery state and attempts, but not the release's
//! approval state (only `release_revision_id`). The header states that gap; the
//! provenance doc records the contract request.

use chrono::NaiveDate;
use contracts::client::HubClient;
use dioxus::prelude::*;

use crate::{
    api,
    components::{Chip, EmptyState, ErrorCard, Loading},
    format, labels,
    shell::ContextItem,
    state::{Store, Workspace},
};

#[component]
pub fn CalendarView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let intents = (store.intents).cloned();
    let accounts = (store.accounts).cloned();
    let mut account_filter = use_signal(|| Option::<String>::None);
    let filter = account_filter();

    let index_client = client.clone();
    use_effect(move || {
        if (store.workspace).cloned() != Workspace::Calendar {
            return;
        }
        let Some(content) = (store.content).cloned().data else { return };
        let client = index_client.clone();
        spawn(async move {
            api::load_release_index(store, client, &content).await;
        });
    });

    let filtered: Vec<contracts::PublicationIntent> = intents
        .data
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|intent| filter.as_ref().is_none_or(|account| &intent.account_id == account))
        .collect();
    let groups = api::group_intents(&filtered);
    let account_label = |id: &str| -> String {
        accounts
            .data
            .as_ref()
            .and_then(|items| items.iter().find(|account| account.id == id))
            .map(|account| format!("{} · {}", account.handle, account.platform))
            .unwrap_or_else(|| id.to_string())
    };

    rsx! {
        aside { class: "context", aria_label: "Calendar accounts",
            div { class: "context-head",
                h2 { "Accounts" }
                span { class: "label-tech", "{accounts.data.as_ref().map(|v| v.len()).unwrap_or(0)}" }
            }
            div { class: "context-list",
                ContextItem {
                    title: "All accounts".to_string(),
                    meta: format!("{} intents", intents.data.as_ref().map(|v| v.len()).unwrap_or(0)),
                    active: filter.is_none(),
                    on_select: move |_| account_filter.set(None),
                }
                if let Some(items) = accounts.data.clone() {
                    for account in items {
                        ContextItem {
                            key: "{account.id}",
                            title: account.handle.clone(),
                            meta: format!("{} · {}", account.platform, account.transport),
                            active: filter.as_ref() == Some(&account.id),
                            on_select: {
                                let id = account.id.clone();
                                move |_| account_filter.set(Some(id.clone()))
                            },
                        }
                    }
                }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "Calendar" }
                    p { class: "hint", "Intents grouped by UTC day and account. Delivery state comes from /v1/intents; approval state is resolved from a bounded release index." }
                }
                div { class: "spacer" }
                button {
                    class: "btn btn-glass",
                    onclick: move |_| {
                        store.clear_release_index();
                        if let Some(content) = (store.content).cloned().data {
                            let client = client.clone();
                            spawn(async move { api::load_release_index(store, client, &content).await; });
                        }
                    },
                    "Refresh index"
                }
            }
            if let Some(error) = intents.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            if let Some(error) = accounts.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match intents.data.clone() {
                None if intents.error.is_none() => rsx! { Loading { text: "Loading intents…".to_string() } },
                None => rsx! {},
                Some(_) if filtered.is_empty() => rsx! {
                    EmptyState {
                        title: "No publication intents.".to_string(),
                        hint: Some("Approved releases become intents when scheduled in Studio.".to_string()),
                    }
                },
                Some(_) => rsx! {
                    for (day, accounts) in groups {
                        {
                            let day_count: usize = accounts.iter().map(|(_, list)| list.len()).sum();
                            rsx! {
                        section { class: "calendar-day", key: "{day}",
                            div { class: "spread",
                                h2 { "{day_heading(&day)}" }
                                span { class: "label-tech", "{day_count} intents" }
                            }
                            for (account_id, list) in accounts {
                                div { class: "glass-card calendar-account", key: "{day}-{account_id}",
                                    div { class: "spread",
                                        span { "{account_label(&account_id)}" }
                                        span { class: "label-tech", "{list.len()} intent(s)" }
                                    }
                                    for intent in list {
                                        {
                                            let (state_label, state_tone) = labels::intent_state(intent.state);
                                            let client = client.clone();
                                            let intent_id = intent.id;
                                            let time = intent.due_at.format("%H:%M").to_string();
                                            let approval = store.release_state(intent.release_revision_id);
                                            let release_content = store.release_content_id(intent.release_revision_id);
                                            rsx! {
                                                div { class: "intent-row", key: "{intent.id}",
                                                    span { class: "time", "{time}" }
                                                    span { class: "mono", "release {format::short_hash(&intent.release_revision_id.to_string())}" }
                                                    Chip { label: state_label.to_string(), tone: Some(state_tone.to_string()) }
                                                    if let Some(approval) = approval {
                                                        {
                                                            let (approval_label, approval_tone) = labels::release_state(approval);
                                                            rsx! { Chip { label: format!("approval: {approval_label}"), tone: Some(approval_tone.to_string()) } }
                                                        }
                                                    } else {
                                                        span { class: "chip muted", "approval: unknown" }
                                                    }
                                                    if let Some(content_id) = release_content {
                                                        button {
                                                            class: "btn btn-glass btn-xs",
                                                            onclick: move |_| crate::actions::open_subject(store, "content", content_id),
                                                            "Open content"
                                                        }
                                                    }
                                                    if let Some(url) = intent.provider_url.clone() {
                                                        button {
                                                            class: "btn btn-glass btn-xs",
                                                            onclick: move |_| api::open_external(&url),
                                                            "Open post"
                                                        }
                                                    }
                                                    if matches!(
                                                        intent.state,
                                                        contracts::IntentState::Scheduled
                                                            | contracts::IntentState::Claimed
                                                            | contracts::IntentState::Submitting
                                                    ) {
                                                        button {
                                                            class: "btn btn-danger-ghost btn-xs",
                                                            onclick: move |_| {
                                                                let client = client.clone();
                                                                spawn(async move {
                                                                    match api::timed(client.cancel_intent(intent_id)).await {
                                                                        Ok(intent) => {
                                                                            store.notify(true, format!("Intent {} is {}.", format::short_hash(&intent.id.to_string()), intent.state));
                                                                            api::refresh_base(store, client).await;
                                                                        }
                                                                        Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                    }
                                                                });
                                                            },
                                                            "Cancel"
                                                        }
                                                    }
                                                }
                                                if !intent.attempts.is_empty() {
                                                    div { class: "attempts",
                                                        for attempt in intent.attempts.iter() {
                                                            {
                                                                let (attempt_label, attempt_tone) = labels::intent_state(attempt.state);
                                                                rsx! {
                                                                    div { class: "attempt", key: "{attempt.id}",
                                                                        span { class: "external", "attempt {attempt.attempt}" }
                                                                        Chip { label: attempt_label.to_string(), tone: Some(attempt_tone.to_string()) }
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
                            }
                        }
                    }
                },
            }
        }
    }
}

fn day_heading(day: &str) -> String {
    NaiveDate::parse_from_str(day, "%Y-%m-%d").map(format::fmt_day).unwrap_or_else(|_| day.to_string())
}
