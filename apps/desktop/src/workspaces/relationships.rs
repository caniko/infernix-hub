//! Relationships: thread, ownership epoch, reply drafts and approval gates.

use contracts::{DraftDecisionIn, DraftReplyIn, DraftState, FollowUpIn, Owner, RelationshipDetail, client::HubClient};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{Chip, EmptyState, ErrorCard, GatePrompt, Loading, Panel},
    format, labels,
    shell::ContextItem,
    state::Store,
};

#[component]
pub fn RelationshipsView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let relationships = (store.relationships).cloned();
    let detail_load = (store.relationship_detail).cloned();
    let selected_id = (store.selected_relationship).cloned();
    let detail: Option<RelationshipDetail> =
        detail_load.data.as_ref().filter(|detail| Some(detail.relationship.id) == selected_id).cloned();

    let mut draft_body = use_signal(String::new);
    let mut in_reply_to = use_signal(|| Option::<Uuid>::None);
    let mut follow_up = use_signal(String::new);
    let mut handoff_url = use_signal(|| Option::<String>::None);
    let mut form_for = use_signal(|| Option::<Uuid>::None);

    use_effect(move || {
        if let Some(id) = (store.selected_relationship).cloned()
            && form_for.cloned() != Some(id)
        {
            draft_body.set(String::new());
            in_reply_to.set(None);
            follow_up.set(String::new());
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

    rsx! {
        aside { class: "context", aria_label: "Relationships",
            div { class: "context-head",
                h2 { "Threads" }
                span { class: "label-tech", "{relationships.data.as_ref().map(|v| v.len()).unwrap_or(0)}" }
            }
            div { class: "context-list",
                match relationships.data.clone() {
                    Some(items) if items.is_empty() => rsx! { p { class: "ctx-empty", "No relationships yet." } },
                    Some(items) => rsx! {
                        for relationship in items {
                            {
                                let (owner_label, _) = labels::owner(relationship.owner);
                                rsx! {
                                    ContextItem {
                                        key: "{relationship.id}",
                                        title: relationship.counterpart.clone(),
                                        meta: format!("{} · {owner_label} · epoch {}", relationship.account_id, relationship.owner_epoch),
                                        active: Some(relationship.id) == selected_id,
                                        on_select: move |_| store.set_selected_relationship(Some(relationship.id)),
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
                None if detail_load.error.is_none() => rsx! { Loading { text: "Loading thread…".to_string() } },
                None => rsx! { EmptyState { title: "Select a relationship.".to_string(), hint: None } },
                Some(detail) => rsx! {
                    {
                        let relationship = &detail.relationship;
                        let relationship_id = relationship.id;
                        let mut interactions = detail.interactions.clone();
                        interactions.sort_by_key(|interaction| interaction.occurred_at);
                        let (owner_label, owner_tone) = labels::owner(relationship.owner);
                        let is_agent = relationship.owner == Owner::Agent;
                        rsx! {
                            div { class: "work-head",
                                div {
                                    h1 { "{relationship.counterpart}" }
                                    p { class: "hint", "{relationship.account_id} · thread {relationship.external_thread_id} · owner epoch {relationship.owner_epoch}" }
                                }
                                div { class: "spacer" }
                                Chip { label: owner_label.to_string(), tone: Some(owner_tone.to_string()) }
                                if is_agent {
                                    button { class: "btn btn-glass", onclick: take_over, "Take over" }
                                } else {
                                    button { class: "btn btn-glass", onclick: release_to_agent, "Release to agent" }
                                }
                                button { class: "btn btn-glass", onclick: handoff, "Open in platform" }
                            }
                            if let Some(url) = handoff_url.cloned() {
                                div { class: "glass-card row",
                                    span { class: "label-tech", "handoff" }
                                    button { class: "btn btn-glass btn-xs", onclick: move |_| api::open_external(&url), "Open again" }
                                    span { class: "mono", "{url}" }
                                }
                            }
                            Panel { title: "Thread".to_string(), caption: Some(format!("{} interactions", interactions.len())),
                                if detail.interactions.is_empty() {
                                    EmptyState { title: "No interactions yet.".to_string(), hint: None }
                                } else {
                                    div { class: "thread",
                                        for interaction in interactions.iter() {
                                            {
                                                let (direction_label, direction_tone) = labels::direction(interaction.direction);
                                                rsx! {
                                                    div {
                                                        class: if interaction.direction == contracts::Direction::Inbound { "interaction inbound" } else { "interaction outbound" },
                                                        key: "{interaction.id}",
                                                        div { class: "meta",
                                                            Chip { label: direction_label.to_string(), tone: Some(direction_tone.to_string()) }
                                                            span { class: "label-tech", "{interaction.author}" }
                                                            span { class: "label-tech", "{format::fmt_dt(interaction.occurred_at)}" }
                                                        }
                                                        div { class: "body", "{interaction.body}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Panel { title: "Draft reply".to_string(), caption: Some(format!("epoch {}", relationship.owner_epoch)),
                                div { class: "form-grid",
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "reply-in-reply-to", "In reply to" }
                                        select {
                                            id: "reply-in-reply-to",
                                            class: "input",
                                            value: "{in_reply_to.cloned().map(|id| id.to_string()).unwrap_or_default()}",
                                            onchange: move |e| in_reply_to.set(e.value().parse().ok()),
                                            option { value: "", "Thread (no specific interaction)" }
                                            for interaction in interactions.iter() {
                                                option { key: "{interaction.id}", value: "{interaction.id}",
                                                    "{interaction.author} · {format::fmt_dt(interaction.occurred_at)}"
                                                }
                                            }
                                        }
                                    }
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "reply-body", "Body" }
                                        textarea {
                                            id: "reply-body", class: "input",
                                            placeholder: "draft the reply…",
                                            value: "{draft_body}",
                                            oninput: move |e| draft_body.set(e.value()),
                                        }
                                    }
                                    div { class: "form-actions",
                                        button { class: "btn btn-primary", onclick: submit_draft, "Draft reply" }
                                    }
                                }
                            }
                            Panel { title: "Drafts".to_string(), caption: Some(format!("{} draft(s)", detail.drafts.len())),
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
                                                            span { class: "label-tech", "{draft.author} · {draft.author_kind} · epoch {draft.owner_epoch}" }
                                                        }
                                                        if needs_gate {
                                                            {
                                                                let client_approve = client.clone();
                                                                let client_reject = client.clone();
                                                                rsx! {
                                                                    GatePrompt {
                                                                        label: format!("approval needed — draft {}", format::short_hash(&draft.id.to_string())),
                                                                        approve_label: "Approve".to_string(),
                                                                        reject_label: "Reject".to_string(),
                                                                        on_approve: move |_| {
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
                                                                        on_reject: move |_| {
                                                                            let client = client_reject.clone();
                                                                            spawn(async move {
                                                                                match api::timed(client.decide_draft(draft_id, &DraftDecisionIn::Reject { reason: None })).await {
                                                                                    Ok(draft) => {
                                                                                        store.notify(true, format!("Draft is now {}.", draft.state));
                                                                                        api::load_relationship_detail(store, client, relationship_id).await;
                                                                                    }
                                                                                    Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                                }
                                                                            });
                                                                        },
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
                                div { class: "form-grid",
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "follow-up-at", "Follow up at (UTC)" }
                                        input {
                                            id: "follow-up-at", class: "input", r#type: "datetime-local",
                                            value: "{follow_up}", oninput: move |e| follow_up.set(e.value()),
                                        }
                                    }
                                    div { class: "form-actions",
                                        button { class: "btn btn-primary", onclick: save_follow_up, "Save follow-up" }
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
