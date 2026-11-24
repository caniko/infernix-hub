//! Calendar: scheduled posts grouped by UTC day and account.

use chrono::NaiveDate;
use contracts::client::HubClient;
use dioxus::prelude::*;

use crate::{
    api,
    components::{Chip, EmptyState, ErrorCard, Loading},
    format, labels,
    shell::ContextItem,
    state::{Loaded, Store, Workspace},
};

#[component]
pub fn CalendarView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let intents = (store.intents).cloned();
    let accounts = (store.accounts).cloned();
    let mut account_filter = use_signal(|| Option::<String>::None);
    let mut refreshing = use_signal(|| false);
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
            .map(|account| format!("{} · {}", account.handle, platform_label(&account.platform)))
            .unwrap_or_else(|| "Unknown account".to_string())
    };
    let scope_account = filter.as_deref().map(account_label).unwrap_or_else(|| "All accounts".to_string());
    let scope_dates = match (groups.first(), groups.last()) {
        (Some((first, _)), Some((last, _))) if first != last => format!("{} — {} · UTC", day_heading(first), day_heading(last)),
        (Some((day, _)), _) => format!("{} · UTC", day_heading(day)),
        _ => "All dates · UTC".to_string(),
    };
    let refresh_client = client.clone();
    let refresh = move |_| {
        if refreshing() {
            return;
        }
        refreshing.set(true);
        let client = refresh_client.clone();
        spawn(async move {
            let mut schedule = store.intents;
            match api::timed(client.intents()).await {
                Ok(items) => {
                    schedule.set(Loaded::ready(items));
                    store.clear_release_index();
                    if let Some(content) = (store.content).cloned().data {
                        api::load_release_index(store, client, &content).await;
                    }
                }
                Err(error) => schedule.write().failed(error),
            }
            refreshing.set(false);
        });
    };

    rsx! {
        aside { class: "context", aria_label: "Calendar accounts",
            div { class: "context-head",
                h2 { class: "row",
                    "Accounts"
                    span { class: "badge-count", "{accounts.data.as_ref().map_or(0, Vec::len)}" }
                }
            }
            div { class: "context-list",
                ContextItem {
                    title: "All accounts".to_string(),
                    meta: post_count(intents.data.as_ref().map_or(0, Vec::len)),
                    active: filter.is_none(),
                    on_select: move |_| account_filter.set(None),
                }
                if let Some(items) = accounts.data.clone() {
                    for account in items {
                        ContextItem {
                            key: "{account.id}",
                            title: account.handle.clone(),
                            meta: format!("{} · {}", platform_label(&account.platform), post_count(
                                intents.data.as_deref().unwrap_or_default().iter().filter(|intent| intent.account_id == account.id).count()
                            )),
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
            div { class: "work-head content-bounded",
                div {
                    h1 { "Scheduled posts by day" }
                    p { class: "form-help", "{scope_account} · {scope_dates}" }
                }
                div { class: "spacer" }
                button {
                    class: "btn secondary",
                    disabled: refreshing(), onclick: refresh,
                    if refreshing() { "Refreshing…" } else { "Refresh schedule" }
                }
            }
            if let Some(error) = intents.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            if let Some(error) = accounts.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match intents.data.clone() {
                None if intents.error.is_none() => rsx! { Loading { text: "Loading schedule…".to_string() } },
                None => rsx! {},
                Some(_) if filtered.is_empty() => rsx! {
                    EmptyState {
                        title: "Nothing scheduled".to_string(),
                        hint: Some("Schedule an approved release in Studio to see it here.".to_string()),
                        actions: rsx! {
                            button { class: "btn primary", onclick: move |_| store.set_workspace(Workspace::Studio), "Schedule in Studio" }
                        },
                    }
                },
                Some(_) => rsx! {
                    for (day, accounts) in groups {
                        {
                            let day_count: usize = accounts.iter().map(|(_, list)| list.len()).sum();
                            rsx! {
                        section { class: "calendar-day content-bounded", key: "{day}",
                            div { class: "spread",
                                h2 { "{day_heading(&day)}" }
                                span { class: "label-tech", "{post_count(day_count)}" }
                            }
                            for (account_id, list) in accounts {
                                div { class: "glass-card calendar-account", key: "{day}-{account_id}",
                                    div { class: "spread",
                                        span { "{account_label(&account_id)}" }
                                        span { class: "label-tech", "{post_count(list.len())}" }
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
                                                            "Open in Studio"
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
                                                                             store.notify(true, format!("Scheduled post is now {}.", intent.state));
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

fn post_count(count: usize) -> String {
    if count == 1 { "1 post".to_string() } else { format!("{count} posts") }
}

fn platform_label(platform: &str) -> &str {
    match platform {
        "mastodon" => "Mastodon",
        "bluesky" => "Bluesky",
        _ => platform,
    }
}
