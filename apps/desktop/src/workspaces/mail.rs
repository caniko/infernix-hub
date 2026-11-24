//! Owner-scoped inbox, triage and business insights.

use std::time::Duration;

use chrono::{TimeDelta, Utc};
use contracts::{MailBiOverview, MailSearchQuery, MailTriageOverride, client::HubClient};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{Chip, EmptyState, ErrorCard, Loading},
    shell::ContextItem,
    state::Store,
};

#[component]
pub fn MailView() -> Element {
    let client = use_context::<HubClient>();
    let store = use_context::<Store>();
    let mut search_input = use_signal(String::new);
    let mut search = use_signal(String::new);
    let mut include_junk = use_signal(|| false);
    let mut classification = use_signal(|| None::<String>);
    let mut selected = use_signal(|| None::<Uuid>);
    let mut insights = use_signal(|| false);
    let mut syncing = use_signal(|| false);

    let accounts_client = client.clone();
    let mut accounts = use_resource(move || {
        let client = accounts_client.clone();
        async move { api::timed(client.list_mail_accounts()).await }
    });
    let overview_client = client.clone();
    let mut overview = use_resource(move || {
        let client = overview_client.clone();
        async move { api::timed(client.mail_bi_overview()).await }
    });
    let messages_client = client.clone();
    let mut messages = use_resource(move || {
        let client = messages_client.clone();
        let q = search();
        let query = MailSearchQuery {
            q: (!q.is_empty()).then_some(q),
            include_junk: include_junk(),
            classification: classification(),
            ..Default::default()
        };
        async move { api::timed(client.list_mail_messages(&query)).await }
    });

    rsx! {
        aside { class: "context", aria_label: "Mail messages",
            div { class: "context-head",
                h2 { "Messages" }
            }
            form { class: "stack", role: "search",
                    onsubmit: move |event| {
                        event.prevent_default();
                        search.set(search_input().trim().to_string());
                    },
                input {
                    class: "input", r#type: "search", aria_label: "Search mail", placeholder: "Search mail", value: search_input(),
                    oninput: move |event| search_input.set(event.value()),
                }
                div { class: "row",
                    button { class: "btn secondary btn-xs", r#type: "submit", "Search" }
                    label { class: "row",
                        input { r#type: "checkbox", checked: include_junk(), onchange: move |event| include_junk.set(event.checked()) }
                        "Include junk"
                    }
                }
            }
            div { class: "context-list",
                match messages.cloned() {
                    None => rsx! { Loading { text: "Loading messages…".to_string() } },
                    Some(Err(e)) => rsx! { ErrorCard { code: e.code, message: e.message } },
                    Some(Ok(items)) if items.is_empty() => rsx! { p { class: "muted", "No matching messages." } },
                    Some(Ok(items)) => rsx! {
                        for item in items {
                            ContextItem {
                                key: "{item.id}",
                                title: item.subject,
                                meta: format!("{} · {} · {}", item.from_addr, item.classification, item.received_at.format("%Y-%m-%d")),
                                active: selected() == Some(item.id) && !insights(),
                                on_select: move |_| { selected.set(Some(item.id)); insights.set(false); },
                            }
                        }
                    },
                }
            }
        }
        main { class: "work stack",
            div { class: "work-head content-bounded",
                div { h1 { "Mail" } }
                div { class: "spacer" }
                button {
                    class: "btn secondary", aria_pressed: insights(),
                    onclick: move |_| insights.toggle(),
                    "BI Insights"
                }
                button {
                    class: "btn secondary", disabled: syncing(),
                    onclick: move |_| {
                        let client = client.clone();
                        syncing.set(true);
                        spawn(async move {
                            match api::timed_for(Duration::from_secs(600), client.sync_mail()).await {
                                Ok(_) => store.notify(true, "Mail sync finished. See account statuses.".to_string()),
                                Err(e) => store.notify(false, format!("{} — {}", e.code, e.message)),
                            }
                            accounts.restart();
                            overview.restart();
                            messages.restart();
                            syncing.set(false);
                        });
                    },
                    if syncing() { "Syncing…" } else { "Sync Now" }
                }
            }
            section { class: "glass-card content-bounded", aria_label: "Mail accounts",
                match accounts.cloned() {
                    None => rsx! { Loading { text: "Loading accounts…".to_string() } },
                    Some(Err(e)) => rsx! { ErrorCard { code: e.code, message: e.message } },
                    Some(Ok(items)) if items.is_empty() => rsx! { p { class: "muted", "No mail accounts enrolled." } },
                    Some(Ok(items)) => rsx! {
                        div { class: "row",
                            for item in items {
                                Chip { key: "{item.id}", label: format!("{} · {}", item.label, item.status), tone: status_tone(&item.status) }
                            }
                        }
                    },
                }
            }
            match overview.cloned() {
                None => rsx! { Loading { text: "Loading business metrics…".to_string() } },
                Some(Err(e)) => rsx! { ErrorCard { code: e.code, message: e.message } },
                Some(Ok(bi)) => rsx! {
                    div { class: "compact-kpi-grid content-bounded",
                        for (label, value) in [
                            ("Messages", bi.total_count.to_string()),
                            ("Signal / junk", format!("{} / {} ({}% signal)", bi.signal_count, bi.junk_count,
                                bi.signal_count.saturating_mul(100).checked_div(bi.total_count).unwrap_or(0))),
                            ("Unread signal", bi.unread_signal_count.to_string()),
                            ("Recurring vendors", bi.recurring_vendor_count.to_string()),
                            ("Open action items", bi.open_action_item_count.to_string()),
                        ] {
                            div { class: "glass-card stat-card", key: "{label}",
                                span { class: "label-tech", "{label}" }
                                strong { class: "value", "{value}" }
                            }
                        }
                    }
                    div { class: "filter-row content-bounded", role: "group", aria_label: "Message classification",
                        button {
                            class: "filter-chip", aria_pressed: classification().is_none(),
                            onclick: move |_| classification.set(None), "All classifications"
                        }
                        for class in bi.by_classification.keys() {
                            button {
                                key: "{class}", class: "filter-chip", aria_pressed: classification().as_ref() == Some(class),
                                onclick: { let class = class.clone(); move |_| classification.set(Some(class.clone())) },
                                "{class}"
                            }
                        }
                    }
                    if insights() {
                        MailInsights { bi, on_select: move |id| { selected.set(Some(id)); insights.set(false); } }
                    }
                },
            }
            if !insights() {
                if let Some(id) = selected() {
                    MessageDetail { key: "{id}", id, on_changed: move |_| { overview.restart(); messages.restart(); } }
                } else {
                    EmptyState { title: "Select a message".to_string() }
                }
            }
        }
    }
}

pub(super) fn status_tone(status: &str) -> Option<String> {
    match status {
        "ok" => Some("ok".to_string()),
        "awaiting_credentials" => Some("warn".to_string()),
        "error" => Some("bad".to_string()),
        _ => None,
    }
}

#[component]
fn MessageDetail(id: Uuid, on_changed: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let store = use_context::<Store>();
    let mut saving = use_signal(|| false);
    let mut updated = use_signal(|| None);
    let detail_client = client.clone();
    let detail = use_resource(move || {
        let client = detail_client.clone();
        async move { api::timed(client.get_mail_message(id)).await }
    });
    let triage = use_callback(move |junk| {
        let client = client.clone();
        saving.set(true);
        spawn(async move {
            let update = MailTriageOverride { is_junk: Some(junk), ..Default::default() };
            match api::timed(client.override_mail_triage(id, &update)).await {
                Ok(value) => {
                    updated.set(Some(value));
                    on_changed.call(());
                }
                Err(e) => store.notify(false, format!("{} — {}", e.code, e.message)),
            }
            saving.set(false);
        });
    });
    let value = match updated().map(Ok).or_else(|| detail.cloned()) {
        None => return rsx! { Loading { text: "Loading message…".to_string() } },
        Some(Err(e)) => return rsx! { ErrorCard { code: e.code, message: e.message } },
        Some(Ok(value)) => value,
    };
    let m = value.message;
    let analysis = value.analysis;
    let date = m.received_at.format("%Y-%m-%d %H:%M UTC");
    let excerpt: String = value.body_text.chars().take(4000).collect();
    rsx! {
        article { class: "glass-card content-bounded",
            h2 { "{m.subject}" }
            p { "{m.from_name} {m.from_addr} · {m.from_domain} · {date}" }
            div { class: "row",
                Chip { label: m.classification }
                Chip { label: format!("Priority: {}", m.priority) }
                Chip { label: format!("Junk score: {:.2}", analysis.junk_score) }
                Chip { label: (if m.is_junk { "Junk" } else { "Not junk" }).to_string() }
            }
            div { class: "row",
                button { class: "btn secondary", disabled: saving() || m.is_junk, onclick: move |_| triage.call(Some(true)), "Mark Junk" }
                button { class: "btn secondary", disabled: saving() || !m.is_junk, onclick: move |_| triage.call(Some(false)), "Not Junk" }
                if analysis.user_override_junk.is_some() {
                    button { class: "btn secondary", disabled: saving(), onclick: move |_| triage.call(None), "Clear override" }
                }
            }
            h3 { "Summary" }
            p { "{analysis.summary}" }
            if let Some(vendor) = analysis.entities.vendor { p { "Vendor: {vendor}" } }
            for amount in analysis.entities.amounts { p { "Amount: {amount.currency} {amount.amount}" } }
            if !analysis.action_items.is_empty() {
                h3 { "Action items" }
                ul { for action in analysis.action_items { li { "{action}" } } }
            }
            h3 { "Body excerpt" }
            p { style: "white-space: pre-wrap; overflow-wrap: anywhere;", "{excerpt}" }
        }
    }
}

#[component]
fn MailInsights(bi: MailBiOverview, on_select: EventHandler<Uuid>) -> Element {
    let client = use_context::<HubClient>();
    let store = use_context::<Store>();
    let mut generating = use_signal(|| false);
    let mut digest = use_signal(|| None::<String>);
    rsx! {
        section { class: "glass-card content-bounded",
            h2 { "Business insights" }
            button {
                class: "btn secondary", disabled: generating(),
                onclick: move |_| {
                    generating.set(true);
                    let client = client.clone();
                    spawn(async move {
                        let end = Utc::now();
                        match api::timed(client.create_mail_bi_digest("weekly", end - TimeDelta::days(7), end)).await {
                            Ok(result) => digest.set(Some(result.summary_markdown)),
                            Err(e) => store.notify(false, format!("{} — {}", e.code, e.message)),
                        }
                        generating.set(false);
                    });
                },
                if generating() { "Generating…" } else { "Generate Digest" }
            }
            if let Some(summary) = digest() {
                h3 { "Digest · last 7 days" }
                p { style: "white-space: pre-wrap;", "{summary}" }
            }
            for (label, items) in [("Top business domains", bi.business_domains), ("Top junk / unsubscribe candidates", bi.unsubscribe_candidates)] {
                h3 { "{label}" }
                if items.is_empty() { p { class: "muted", "None detected." } }
                ul { for item in items { li { key: "{item.key}", "{item.key} · {item.message_count}" } } }
            }
            h3 { "Recurring vendors" }
            if bi.recurring_vendors.is_empty() { p { class: "muted", "None detected." } }
            ul { for item in bi.recurring_vendors { li { key: "{item.vendor}", "{item.vendor} · {item.message_count}" } } }
            h3 { "Open action items" }
            if bi.open_action_items.is_empty() { p { class: "muted", "None detected." } }
            ul {
                for item in bi.open_action_items {
                    li {
                        button { class: "btn btn-glass btn-xs", onclick: move |_| on_select.call(item.message_id), "{item.subject}" }
                        " {item.action}"
                    }
                }
            }
        }
    }
}
