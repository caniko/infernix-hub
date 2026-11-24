//! Relationships: thread, ownership epoch, reply drafts and approval gates.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use contracts::{DraftDecisionIn, DraftReplyIn, DraftState, FollowUpIn, Owner, Relationship, RelationshipDetail, client::HubClient};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    actions, api,
    components::{Chip, EmptyState, ErrorCard, Loading, Panel},
    format, labels,
    state::Store,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    NeedsYou,
    All,
}

/// Whether the operator owes this thread attention: a human-owned thread, a due
/// follow-up, or a thread the Today queue lists as an overdue interaction (the
/// server's rule, which also knows whether a reply already went out).
pub fn needs_you(relationship: &Relationship, now: DateTime<Utc>, overdue: &[Uuid]) -> bool {
    relationship.owner == Owner::Human || relationship.follow_up_at.is_some_and(|at| at <= now) || overdue.contains(&relationship.id)
}

/// Rows in display order for one filter: most recent inbound first, threads that
/// never saw an inbound last.
pub fn rows(items: &[Relationship], filter: Filter, now: DateTime<Utc>, overdue: &[Uuid]) -> Vec<Relationship> {
    let mut rows: Vec<Relationship> = items.iter().filter(|item| filter == Filter::All || needs_you(item, now, overdue)).cloned().collect();
    rows.sort_by_key(|item| std::cmp::Reverse(item.last_inbound_at));
    rows
}

fn attention_label(relationship: &Relationship, now: DateTime<Utc>, overdue: &[Uuid]) -> (String, &'static str) {
    if relationship.follow_up_at.is_some_and(|at| at <= now) {
        ("Follow-up due".to_string(), "warn")
    } else if overdue.contains(&relationship.id) {
        let waiting = relationship
            .last_inbound_at
            .map(|at| format::rel_time_at(now.timestamp_millis().max(0) as u64, at.timestamp_millis().max(0) as u64));
        let label = waiting
            .as_deref()
            .and_then(|time| time.strip_suffix(" ago"))
            .map_or_else(|| "Overdue reply".to_string(), |elapsed| format!("Overdue reply · {elapsed} waiting"));
        (label, "warn")
    } else {
        let (label, tone) = labels::owner(relationship.owner);
        (label.to_string(), tone)
    }
}

fn incoming_preview(detail: &RelationshipDetail) -> String {
    detail
        .interactions
        .iter()
        .filter(|message| message.direction == contracts::Direction::Inbound)
        .max_by_key(|message| message.occurred_at)
        .map(|message| format::preview(&message.body, 90))
        .unwrap_or_default()
}

/// Assistant drafting waits for inference; the hub's assistant transport allows
/// 180 s, so the console waits that long plus a margin.
const ASSISTANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(190);

#[component]
pub fn RelationshipsView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let relationships = (store.relationships).cloned();
    let accounts = (store.accounts).cloned();
    let detail_load = (store.relationship_detail).cloned();
    let selected_id = (store.selected_relationship).cloned();
    let detail: Option<RelationshipDetail> =
        detail_load.data.as_ref().filter(|detail| Some(detail.relationship.id) == selected_id).cloned();

    let mut draft_body = use_signal(String::new);
    let mut in_reply_to = use_signal(|| Option::<Uuid>::None);
    let mut follow_up = use_signal(String::new);
    let mut reject_reasons = use_signal(HashMap::<Uuid, String>::new);
    let mut handoff_url = use_signal(|| Option::<String>::None);
    let mut form_for = use_signal(|| Option::<Uuid>::None);
    let mut filter = use_signal(|| None::<Filter>);
    let mut assistant_busy = use_signal(|| false);
    let overdue = use_memo(move || {
        store
            .today
            .read()
            .data
            .as_ref()
            .map(|today| {
                today
                    .overdue_interactions
                    .iter()
                    .filter(|item| item.subject_type == "relationship")
                    .map(|item| item.subject_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let visible = use_memo(move || {
        let data = store.relationships.read();
        let items = data.data.as_ref()?;
        let now = Utc::now();
        let preferred = filter().unwrap_or(Filter::NeedsYou);
        let threads = rows(items, preferred, now, &overdue.read());
        if filter().is_none()
            && (threads.is_empty() || store.selected_relationship.cloned().is_some_and(|id| !threads.iter().any(|thread| thread.id == id)))
        {
            Some((Filter::All, rows(items, Filter::All, now, &[])))
        } else {
            Some((preferred, threads))
        }
    });
    use_effect(move || {
        let visible = visible.read();
        if let Some((current, items)) = visible.as_ref()
            && let Some(first) = items.first()
        {
            if filter().is_none() {
                filter.set(Some(*current));
            }
            if store.selected_relationship.read().is_none() {
                store.set_selected_relationship(Some(first.id));
            }
        }
    });

    // The list response has no message text. Prefetch only the first screenful,
    // sequentially and only when its inbound timestamps change.
    // ponytail: rows past this budget show a preview after opening the thread.
    let preview_targets = use_memo(move || {
        let selected = (store.selected_relationship).cloned();
        visible
            .read()
            .as_ref()
            .map(|(_, items)| {
                items
                    .iter()
                    .take(8)
                    .filter(|thread| Some(thread.id) != selected)
                    .filter_map(|thread| Some((thread.id, thread.last_inbound_at?)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let mut previews = use_signal(HashMap::<Uuid, (DateTime<Utc>, String)>::new);
    let preview_client = client.clone();
    use_resource(move || {
        let targets = preview_targets();
        let client = preview_client.clone();
        async move {
            for (id, at) in targets {
                if previews.peek().get(&id).is_some_and(|(loaded_at, _)| *loaded_at == at) {
                    continue;
                }
                if let Ok(detail) = api::timed(client.relationship(id)).await
                    && detail.relationship.last_inbound_at == Some(at)
                {
                    previews.write().insert(id, (at, incoming_preview(&detail)));
                }
            }
        }
    });
    use_effect(move || {
        if let Some(detail) = store.relationship_detail.read().data.as_ref()
            && let Some(at) = detail.relationship.last_inbound_at
        {
            let id = detail.relationship.id;
            let preview = (at, incoming_preview(detail));
            if previews.peek().get(&id) != Some(&preview) {
                previews.write().insert(id, preview);
            }
        }
    });

    use_effect(move || {
        if let Some(id) = (store.selected_relationship).cloned()
            && form_for.cloned() != Some(id)
            && let Some(detail) = (store.relationship_detail).cloned().data.filter(|detail| detail.relationship.id == id)
        {
            draft_body.set(String::new());
            in_reply_to.set(None);
            follow_up.set(detail.relationship.follow_up_at.map(format::default_local).unwrap_or_default());
            reject_reasons.write().clear();
            handoff_url.set(None);
            form_for.set(Some(id));
        }
    });

    let client_submit_draft = client.clone();
    let submit_draft = move |_| {
        let Some(detail) = (store.relationship_detail)
            .cloned()
            .data
            .filter(|detail| Some(detail.relationship.id) == (store.selected_relationship).cloned())
        else {
            return;
        };
        let body = draft_body.cloned().trim().to_string();
        if body.is_empty() {
            store.notify(false, "Draft body cannot be empty.");
            return;
        }
        let input = DraftReplyIn { in_reply_to: in_reply_to.cloned(), body, owner_epoch: detail.relationship.owner_epoch };
        let relationship_id = detail.relationship.id;
        let client_submit_draft = client_submit_draft.clone();
        spawn(async move {
            match api::timed(client_submit_draft.draft_reply(relationship_id, &input)).await {
                Ok(draft) => {
                    store.notify(true, format!("Draft {} created.", format::short_hash(&draft.id.to_string())));
                    draft_body.set(String::new());
                    api::load_relationship_detail(store, client_submit_draft, relationship_id).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_ask_assistant = client.clone();
    let ask_assistant = move |_| {
        let Some(id) = (store.selected_relationship).cloned() else { return };
        if assistant_busy.cloned() {
            return;
        }
        assistant_busy.set(true);
        let client_ask_assistant = client_ask_assistant.clone();
        spawn(async move {
            match api::timed_for(ASSISTANT_TIMEOUT, client_ask_assistant.draft_assistant_reply(id)).await {
                Ok(_) => {
                    store.notify(true, "Assistant drafted a reply for review.");
                    api::load_relationship_detail(store, client_ask_assistant, id).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
            assistant_busy.set(false);
        });
    };

    let client_take_over = client.clone();
    let take_over = move |_| {
        let Some(id) = (store.selected_relationship).cloned() else { return };
        let client_take_over = client_take_over.clone();
        spawn(async move {
            match api::timed(client_take_over.takeover(id)).await {
                Ok(relationship) => {
                    store.notify(true, format!("Owner is now {}.", relationship.owner));
                    api::load_relationship_detail(store, client_take_over, id).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_release_to_agent = client.clone();
    let release_to_agent = move |_| {
        let Some(id) = (store.selected_relationship).cloned() else { return };
        let client_release_to_agent = client_release_to_agent.clone();
        spawn(async move {
            match api::timed(client_release_to_agent.release_to_agent(id)).await {
                Ok(relationship) => {
                    store.notify(true, format!("Owner is now {}.", relationship.owner));
                    api::load_relationship_detail(store, client_release_to_agent, id).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_handoff = client.clone();
    let handoff = move |_| {
        let Some(id) = (store.selected_relationship).cloned() else { return };
        let client_handoff = client_handoff.clone();
        spawn(async move {
            match api::timed(client_handoff.handoff(id)).await {
                Ok(handoff) => {
                    handoff_url.set(Some(handoff.url.clone()));
                    api::open_external(&handoff.url);
                    store.notify(true, format!("Handoff opened: {}", handoff.url));
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_save_follow_up = client.clone();
    let save_follow_up = move |_| {
        let Some(id) = (store.selected_relationship).cloned() else { return };
        let follow_up_at = if follow_up.cloned().trim().is_empty() {
            None
        } else {
            match format::parse_local(&follow_up.cloned()) {
                Some(at) => Some(at),
                None => {
                    store.notify(false, "Follow-up needs a valid date/time.");
                    return;
                }
            }
        };
        let client_save_follow_up = client_save_follow_up.clone();
        spawn(async move {
            let path = format!("/relationships/{id}/follow-up");
            match api::timed(client_save_follow_up.post::<FollowUpIn, contracts::Relationship>(&path, &FollowUpIn { follow_up_at })).await {
                Ok(_) => {
                    store.notify(true, "Follow-up updated.");
                    api::load_relationship_detail(store, client_save_follow_up, id).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_clear_follow_up = client.clone();
    let clear_follow_up = move |_| {
        let Some(id) = (store.selected_relationship).cloned() else { return };
        let client_clear_follow_up = client_clear_follow_up.clone();
        follow_up.set(String::new());
        spawn(async move {
            let path = format!("/relationships/{id}/follow-up");
            match api::timed(client_clear_follow_up.post::<FollowUpIn, contracts::Relationship>(&path, &FollowUpIn { follow_up_at: None }))
                .await
            {
                Ok(_) => {
                    store.notify(true, "Follow-up cleared.");
                    api::load_relationship_detail(store, client_clear_follow_up, id).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let visible_threads = visible.read();
    let current = visible_threads.as_ref().map_or_else(|| filter().unwrap_or(Filter::NeedsYou), |(current, _)| *current);
    let list = visible_threads.as_ref().map(|(_, items)| items.as_slice());
    let now = Utc::now();
    let overdue_items = overdue.read();
    let previews = previews.read();
    let none_at_all = relationships.data.as_ref().is_some_and(Vec::is_empty);
    let channel = |relationship: &Relationship| {
        accounts
            .data
            .as_ref()
            .and_then(|items| items.iter().find(|account| account.id == relationship.account_id))
            .map_or_else(|| "Unknown channel".to_string(), |account| format!("{} · {}", account.platform, account.handle))
    };

    rsx! {
        aside { class: "context", aria_label: "Relationships",
            div { class: "context-head",
                h2 { "Threads" }
                span { class: "label-tech", "{relationships.data.as_ref().map(|v| v.len()).unwrap_or(0)}" }
            }
            div { class: "segmented", role: "group", aria_label: "Filter threads", style: "margin-bottom: 8px;",
                button {
                    class: "seg-btn", aria_pressed: current == Filter::NeedsYou,
                    onclick: move |_| filter.set(Some(Filter::NeedsYou)),
                    "Needs you"
                }
                button {
                    class: "seg-btn", aria_pressed: current == Filter::All,
                    onclick: move |_| filter.set(Some(Filter::All)),
                    "All"
                }
            }
            div { class: "context-list", style: "gap: 6px;",
                match list {
                    Some([]) => {
                        let text = if none_at_all { "No conversations yet." } else { "Nothing needs you." };
                        rsx! { p { class: "ctx-empty", "{text}" } }
                    }
                    Some(items) => rsx! {
                        for relationship in items {
                            {
                                let (status, tone) = attention_label(relationship, now, &overdue_items);
                                let id = relationship.id;
                                let active = Some(id) == selected_id;
                                let preview = previews.get(&id)
                                    .filter(|(at, text)| Some(*at) == relationship.last_inbound_at && !text.is_empty())
                                    .map(|(_, text)| text.as_str());
                                rsx! {
                                    button {
                                        key: "{id}",
                                        class: if active { "context-item active" } else { "context-item" },
                                        style: "box-shadow: inset 0 0 0 1px var(--line-subtle); overflow-wrap: anywhere;",
                                        aria_pressed: active,
                                        onclick: move |_| store.set_selected_relationship(Some(id)),
                                        strong { class: "ctx-title", "{relationship.counterpart}" }
                                        span { class: "ctx-meta", "{channel(relationship)}" }
                                        Chip { label: status, tone: Some(tone.to_string()) }
                                        if let Some(preview) = preview {
                                            span { class: "ctx-meta", "{preview}" }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    None if relationships.error.is_none() => rsx! { p { class: "ctx-empty", "Loading threads…" } },
                    None => rsx! {},
                }
            }
        }
        main { class: "work",
            if let Some(error) = relationships.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            if let Some(error) = detail_load.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match detail {
                None if none_at_all => rsx! {
                    EmptyState {
                        title: "No conversations yet.".to_string(),
                        hint: Some("Replies, comments, and direct messages from connected channels appear here.".to_string()),
                    }
                },
                None if selected_id.is_none() && list.is_some_and(|items| items.is_empty()) => rsx! {
                    EmptyState {
                        title: "Nothing needs you".to_string(),
                        hint: Some("Switch to All to see conversations handled by the agent.".to_string()),
                        actions: rsx! {
                            button { class: "btn secondary", onclick: move |_| filter.set(Some(Filter::All)), "Show all threads" }
                        },
                    }
                },
                None if selected_id.is_none() && relationships.loading => rsx! { Loading { text: "Loading threads…".to_string() } },
                None if detail_load.error.is_none() && (list.is_some() || selected_id.is_some()) => rsx! {
                    Loading { text: "Loading thread…".to_string() }
                },
                None => rsx! {},
                Some(detail) => rsx! {
                    {
                        let relationship = &detail.relationship;
                        let relationship_id = relationship.id;
                        let mut interactions = detail.interactions.clone();
                        interactions.sort_by_key(|interaction| interaction.occurred_at);
                        let (owner_label, owner_tone) = labels::owner(relationship.owner);
                        let is_agent = relationship.owner == Owner::Agent;
                        let message_count = interactions.len();
                        let draft_count = detail.drafts.len();
                        rsx! {
                            div { class: "work-head", style: "margin-bottom: 8px;",
                                div {
                                    h1 { "{relationship.counterpart}" }
                                    p { class: "hint", "{channel(relationship)}" }
                                }
                                div { class: "spacer" }
                                div { class: "row",
                                    Chip { label: owner_label.to_string(), tone: Some(owner_tone.to_string()) }
                                    if is_agent {
                                        span { class: "form-help", id: "relationship-ownership-help",
                                            "No human assignee — take over to reply directly, or draft for review."
                                        }
                                    } else {
                                        span { class: "form-help",
                                            if let Some(name) = relationship.owner_user.as_deref() { "Assigned to {name}" } else { "No human assignee" }
                                        }
                                    }
                                }
                            }
                            div { class: "toolbar", style: "margin-block: 6px 10px;",
                                if is_agent {
                                    button { class: "btn secondary", aria_describedby: "relationship-ownership-help", onclick: take_over, "Take over" }
                                } else {
                                    button { class: "btn secondary", onclick: release_to_agent, "Release to agent" }
                                }
                                if let Some(url) = relationship.permalink.clone() {
                                    button { class: "btn ghost", onclick: move |_| api::open_external(&url), "View post ↗" }
                                    if let Some(url) = handoff_url.cloned() {
                                        button { class: "btn ghost", onclick: move |_| api::open_external(&url), "Open in platform ↗" }
                                    } else {
                                        button { class: "btn ghost", title: "Hand off replies to the platform", onclick: handoff, "Open in platform ↗" }
                                    }
                                }
                                if let Some(content_id) = relationship.content_item_id {
                                    button { class: "btn ghost", onclick: move |_| actions::open_subject(store, "content", content_id), "Open in Studio" }
                                }
                            }
                            section { class: "panel", style: "gap: 8px; margin-bottom: 12px;",
                                div { class: "panel-head",
                                    h3 { "Thread" }
                                    span { class: "form-help nowrap-unit", if message_count == 1 { "1 message" } else { "{message_count} messages" } }
                                }
                                if detail.interactions.is_empty() {
                                    EmptyState { title: "No messages yet.".to_string(), hint: None }
                                } else {
                                    div { class: "thread",
                                        for interaction in interactions.iter() {
                                            {
                                                let (direction_label, direction_tone) = labels::direction(interaction.direction);
                                                rsx! {
                                                    div {
                                                        class: if interaction.direction == contracts::Direction::Inbound { "interaction inbound" } else { "interaction outbound" },
                                                        key: "{interaction.id}",
                                                        div { class: "meta row", style: "overflow-wrap: anywhere;",
                                                            Chip { label: direction_label.to_string(), tone: Some(direction_tone.to_string()) }
                                                            span { class: "form-help", "{labels::author(&interaction.author)}" }
                                                            span { class: "form-help nowrap-unit", "{format::fmt_dt(interaction.occurred_at)}" }
                                                        }
                                                        div { class: "body", "{interaction.body}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Panel { title: "Draft reply".to_string(), caption: None,
                                div { class: "form-stack", style: "gap: 8px;",
                                    div { class: "form-field",
                                        label { class: "label-tech", r#for: "reply-in-reply-to", "In reply to" }
                                        select {
                                            id: "reply-in-reply-to",
                                            class: "input",
                                            value: "{in_reply_to.cloned().map(|id| id.to_string()).unwrap_or_default()}",
                                            onchange: move |e| in_reply_to.set(e.value().parse().ok()),
                                            option { value: "", "Thread (no specific message)" }
                                            for interaction in interactions.iter() {
                                                option { key: "{interaction.id}", value: "{interaction.id}",
                                                    "{labels::author(&interaction.author)} · {format::fmt_dt(interaction.occurred_at)}"
                                                }
                                            }
                                        }
                                    }
                                    div { class: "form-field",
                                        label { class: "label-tech", r#for: "reply-body", "Reply" }
                                        textarea {
                                            id: "reply-body", class: "input", rows: "2", style: "min-height: 56px;",
                                            placeholder: "draft the reply…",
                                            value: "{draft_body}",
                                            oninput: move |e| draft_body.set(e.value()),
                                        }
                                    }
                                    div { class: "row",
                                        button { class: "btn primary", onclick: submit_draft, "Draft reply" }
                                        button {
                                            class: "btn secondary",
                                            disabled: assistant_busy.cloned(),
                                            onclick: ask_assistant,
                                            if assistant_busy.cloned() { "Drafting…" } else { "Ask assistant" }
                                        }
                                    }
                                }
                            }
                            section { class: "panel",
                                div { class: "panel-head",
                                    h3 { "Drafts" }
                                    span { class: "form-help nowrap-unit", if draft_count == 1 { "1 draft" } else { "{draft_count} drafts" } }
                                }
                                if detail.drafts.is_empty() {
                                    EmptyState { title: "No drafts.".to_string(), hint: Some("Draft a reply above; approval is required before it is sent.".to_string()) }
                                } else {
                                    div { class: "stack",
                                        for draft in detail.drafts.iter() {
                                            {
                                                let (state_label, state_tone) = labels::draft_state(draft.state);
                                                let draft_id = draft.id;
                                                let client = client.clone();
                                                let needs_gate = matches!(draft.state, DraftState::Draft | DraftState::InReview);
                                                rsx! {
                                                    div { class: "glass-card draft", key: "{draft.id}",
                                                        p { "{draft.body}" }
                                                        div { class: "row",
                                                            Chip { label: state_label.to_string(), tone: Some(state_tone.to_string()) }
                                                            span { class: "form-help", "{labels::author(&draft.author)}" }
                                                        }
                                                        if needs_gate {
                                                            {
                                                                let client_approve = client.clone();
                                                                let client_reject = client.clone();
                                                                rsx! {
                                                                    div { class: "form-row", style: "align-items: end;",
                                                                        div { class: "form-field",
                                                                            label { class: "label-tech", r#for: "reject-{draft_id}", "Rejection note (optional)" }
                                                                            input {
                                                                                id: "reject-{draft_id}", class: "input",
                                                                                value: reject_reasons.read().get(&draft_id).cloned().unwrap_or_default(),
                                                                                oninput: move |e| { reject_reasons.write().insert(draft_id, e.value()); },
                                                                            }
                                                                        }
                                                                        div { class: "row",
                                                                            button { class: "btn secondary",
                                                                                onclick: move |_| {
                                                                                    let client = client_approve.clone();
                                                                                    spawn(async move {
                                                                                        match api::timed(client.decide_draft(draft_id, &DraftDecisionIn::Approve)).await {
                                                                                            Ok(draft) => {
                                                                                                store.notify(true, format!("Draft is now {}.", draft.state));
                                                                                                api::load_relationship_detail(store, client, relationship_id).await;
                                                                                            }
                                                                                            Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                                        }
                                                                                    });
                                                                                },
                                                                                "Approve"
                                                                            }
                                                                            button { class: "btn danger",
                                                                                onclick: move |_| {
                                                                                    let reason = reject_reasons.read().get(&draft_id).map(|reason| reason.trim())
                                                                                        .filter(|reason| !reason.is_empty()).map(str::to_owned);
                                                                                    let client = client_reject.clone();
                                                                                    spawn(async move {
                                                                                        match api::timed(client.decide_draft(draft_id, &DraftDecisionIn::Reject { reason })).await {
                                                                                            Ok(draft) => {
                                                                                                store.notify(true, format!("Draft is now {}.", draft.state));
                                                                                                api::load_relationship_detail(store, client, relationship_id).await;
                                                                                            }
                                                                                            Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                                        }
                                                                                    });
                                                                                },
                                                                                "Reject"
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
                            Panel { title: "Follow-up".to_string(), caption: Some("relationship reminder".to_string()),
                                div { class: "form-stack",
                                    div { class: "form-field", style: "max-width: 20rem;",
                                        label { class: "label-tech", r#for: "follow-up-at", "Follow up at (UTC)" }
                                        input {
                                            id: "follow-up-at", class: "input", r#type: "datetime-local",
                                            aria_describedby: follow_up.read().is_empty().then_some("follow-up-hint"),
                                            value: if form_for() == Some(relationship_id) { follow_up.cloned() } else { String::new() },
                                            oninput: move |e| follow_up.set(e.value()),
                                        }
                                        if follow_up.read().is_empty() {
                                            p { class: "form-help", id: "follow-up-hint", "Choose a date and time." }
                                        }
                                    }
                                    div { class: "row", style: "gap: 10px;",
                                        button { class: "btn secondary", onclick: save_follow_up, "Save follow-up" }
                                        button { class: "btn secondary", onclick: clear_follow_up, "Clear" }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 17, hour, 0, 0).unwrap()
    }

    fn relationship(owner: Owner, follow_up_at: Option<DateTime<Utc>>, last_inbound_at: Option<DateTime<Utc>>) -> Relationship {
        Relationship {
            id: Uuid::new_v4(),
            account_id: "studio-ig".into(),
            external_thread_id: "thread-1".into(),
            counterpart: "someone".into(),
            owner,
            owner_user: None,
            owner_epoch: 1,
            follow_up_at,
            last_inbound_at,
            content_item_id: None,
            permalink: None,
        }
    }

    #[test]
    fn needs_you_follows_ownership_follow_up_and_inbound_age() {
        let now = at(12);
        assert!(needs_you(&relationship(Owner::Human, None, Some(at(11))), now, &[]), "a human-owned thread always needs the operator");
        assert!(needs_you(&relationship(Owner::Agent, Some(at(12)), Some(at(11))), now, &[]), "a due follow-up needs the operator");
        let stale = relationship(Owner::Agent, None, Some(at(7)));
        assert!(needs_you(&stale, now, &[stale.id]), "a thread Today lists as overdue needs the operator");
        assert!(!needs_you(&stale, now, &[]), "an answered old inbound (absent from Today) does not");
        assert_eq!(attention_label(&stale, now, &[stale.id]), ("Overdue reply · 5h waiting".into(), "warn"));
        assert_eq!(attention_label(&stale, now, &[]), ("Agent owned".into(), "info"));
        let follow_up = relationship(Owner::Agent, Some(at(12)), Some(at(11)));
        assert_eq!(attention_label(&follow_up, now, &[follow_up.id]), ("Follow-up due".into(), "warn"));
        assert!(!needs_you(&relationship(Owner::Agent, Some(at(13)), None), now, &[]), "a future follow-up does not");

        let items = vec![
            relationship(Owner::Agent, None, None),
            relationship(Owner::Agent, None, Some(at(11))),
            relationship(Owner::Agent, None, Some(at(7))),
        ];
        let ordered = rows(&items, Filter::All, now, &[]);
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].last_inbound_at, Some(at(11)), "newest inbound leads");
        assert_eq!(ordered[2].last_inbound_at, None, "a thread without inbound goes last");
        assert_eq!(rows(&items, Filter::NeedsYou, now, &[items[2].id]).len(), 1, "only the overdue thread needs the operator here");
    }
}
