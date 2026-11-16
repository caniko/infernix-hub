//! Influencers, characters and proposal-only canon review.
//!
//! Canon is proposal-only: the rules editor submits `CanonProposalIn`; accepting
//! a proposal is the only path that changes accepted canon.

use contracts::{
    CanonBody, CanonDecisionIn, CanonProposalIn, CanonRevision, CanonSource, CanonState, ChatMessageIn, InfluencerIn, client::HubClient,
};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{
        Chip, EmptyState, ErrorCard, GatePrompt, Loading, Panel,
        chat::{ChatPanel, TranscriptLine},
        proposal::{DeltaRow, ListEditor, ProposalCard},
    },
    format, labels,
    shell::ContextItem,
    state::{Loaded, Store},
};

#[component]
pub fn InfluencersView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut show_character = store.influencers_show_character;
    let mut show_new = use_signal(|| false);
    let mut wizard = use_signal(|| None::<(Option<Uuid>, Option<Uuid>)>);
    let mut pending_accept = use_signal(|| None::<(Uuid, Uuid, CanonDecisionIn)>);
    let influencers = (store.influencers).cloned();
    let selected_influencer_id = (store.selected_influencer).cloned();
    let influencer = influencers.data.as_ref().and_then(|items| items.iter().find(|item| Some(item.id) == selected_influencer_id).cloned());
    let mut premise = use_signal(String::new);
    let mut dynamics = use_signal(String::new);
    let mut influencer_form_for = use_signal(|| Option::<Uuid>::None);
    let mut save_error = use_signal(|| Option::<crate::state::UiError>::None);
    use_effect(move || {
        let id = (store.selected_influencer).cloned();
        if influencer_form_for.cloned() == id {
            return;
        }
        if let Some(item) = (store.influencers).cloned().data.and_then(|items| items.into_iter().find(|item| Some(item.id) == id)) {
            premise.set(item.premise);
            dynamics.set(item.dynamics);
            influencer_form_for.set(id);
            save_error.set(None);
        }
    });
    let personas = (store.personas).cloned();
    let selected_persona_id = (store.selected_persona).cloned();
    let persona = personas.data.as_ref().and_then(|items| items.iter().find(|p| Some(p.id) == selected_persona_id).cloned());
    let canon_load = (store.canon).cloned();
    let revisions: Vec<CanonRevision> = canon_load
        .data
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|revision| Some(revision.persona_id) == selected_persona_id)
        .collect();

    let mut voice = use_signal(String::new);
    let mut rules = use_signal(Vec::<String>::new);
    let mut avoid = use_signal(Vec::<String>::new);
    let mut world = use_signal(Vec::<String>::new);
    let mut rationale = use_signal(String::new);
    let mut form_for = use_signal(|| Option::<Uuid>::None);

    use_effect(move || {
        let Some(persona_id) = (store.selected_persona).cloned() else { return };
        let Some(revisions) = (store.canon).cloned().data else { return };
        if form_for.cloned() == Some(persona_id) {
            return;
        }
        let accepted = revisions.iter().find(|revision| revision.state == CanonState::Accepted && revision.persona_id == persona_id);
        voice.set(accepted.map(|r| r.body.voice.clone()).unwrap_or_default());
        rules.set(accepted.map(|r| r.body.rules.clone()).unwrap_or_default());
        avoid.set(accepted.map(|r| r.body.avoid.clone()).unwrap_or_default());
        world.set(accepted.map(|r| r.body.world.clone()).unwrap_or_default());
        rationale.set(String::new());
        form_for.set(Some(persona_id));
    });

    let client_submit_proposal = client.clone();
    let submit_proposal = move |_| {
        let Some(persona_id) = (store.selected_persona).cloned() else { return };
        let base_revision = (store.canon).cloned().data.as_ref().and_then(|revisions| {
            revisions.iter().filter(|revision| revision.state == CanonState::Accepted).map(|revision| revision.revision).max()
        });
        let body = CanonBody { voice: voice.cloned(), rules: rules.cloned(), avoid: avoid.cloned(), world: world.cloned() };
        let rationale_value = rationale.cloned().trim().to_string();
        let input = CanonProposalIn {
            base_revision,
            body,
            source: CanonSource::Operator,
            rationale: if rationale_value.is_empty() { None } else { Some(rationale_value) },
        };
        let client_submit_proposal = client_submit_proposal.clone();
        spawn(async move {
            match api::timed(client_submit_proposal.propose_canon(persona_id, &input)).await {
                Ok(revision) => {
                    store.notify(true, format!("Canon proposal revision {} submitted for review.", revision.revision));
                    api::load_canon(store, client_submit_proposal.clone(), persona_id).await;
                    api::refresh_base(store, client_submit_proposal).await;
                    form_for.set(None);
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_send_chat = client.clone();
    let send_chat = move |text: String| {
        let Some(persona_id) = (store.selected_persona).cloned() else { return };
        let client_send_chat = client_send_chat.clone();
        spawn(async move {
            match api::timed(client_send_chat.send_chat("persona", persona_id, &ChatMessageIn { body: text })).await {
                Ok(messages) => store.set_chat_persona(Loaded::ready(messages)),
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let review_from_chat = move |index: usize| {
        let Some(messages) = (store.chat_persona).cloned().data else { return };
        let Some(message) = messages.get(index) else { return };
        if let Some(proposal) = message.proposal_id {
            store.set_focus_proposal(Some(proposal));
        }
    };

    rsx! {
        aside { class: "context", aria_label: "Influencers and characters",
            div { class: "context-head",
                h2 { "Influencers" }
                button { class: "btn btn-glass", onclick: move |_| show_new.set(true), "New influencer" }
                button { class: "btn btn-glass", onclick: move |_| wizard.set(Some((None, (store.selected_influencer).cloned()))), "New character" }
            }
            div { class: "context-list",
                match influencers.data.clone() {
                    Some(items) if items.is_empty() => rsx! { p { class: "ctx-empty", "No influencers yet." } },
                    Some(items) => rsx! {
                        for item in items {
                            {
                                let (status, _) = influencer_status(&item);
                                let kind = match item.kind { contracts::InfluencerKind::Solo => "Solo", contracts::InfluencerKind::Group => "Group" };
                                rsx! { ContextItem {
                                    key: "{item.id}", title: item.name.clone(),
                                    meta: format!("{} · {} characters · {}", kind, item.members.len(), status),
                                    active: !show_character.cloned() && Some(item.id) == selected_influencer_id,
                                    on_select: move |_| { let mut selected = store.selected_influencer; selected.set(Some(item.id)); show_character.set(false); },
                                } }
                            }
                        }
                    },
                    None if influencers.error.is_none() => rsx! { p { class: "ctx-empty", "Loading influencers…" } },
                    None => rsx! {},
                }
                h2 { "Characters" }
                match personas.data.clone() {
                    Some(items) if items.is_empty() => rsx! { p { class: "ctx-empty", "No personas yet." } },
                    Some(items) => rsx! {
                        for item in items {
                            ContextItem {
                                key: "{item.id}",
                                title: item.name.clone(),
                                meta: format!(
                                    "canon {} · {} pending",
                                    item.canon.as_ref().map(|c| c.revision.to_string()).unwrap_or_else(|| "—".into()),
                                    item.pending_proposals
                                ),
                                active: show_character.cloned() && Some(item.id) == selected_persona_id,
                                on_select: {
                                    move |_| {
                                        store.set_selected_persona(Some(item.id));
                                        form_for.set(None);
                                    }
                                },
                            }
                        }
                    },
                    None if personas.error.is_none() => rsx! { p { class: "ctx-empty", "Loading personas…" } },
                    None => rsx! {},
                }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "Influencers" }
                    p { class: "hint", "Persona canon is proposal-only: the assistant proposes, the operator accepts." }
                }
            }
            if let Some(error) = influencers.error.clone() { ErrorCard { code: error.code, message: error.message } }
            if let Some(error) = personas.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match show_character.cloned() {
                true => rsx! {
                    if let Some(error) = canon_load.error.clone() {
                        ErrorCard { code: error.code, message: error.message }
                    }
                    match persona {
                        None if personas.error.is_none() => rsx! { Loading { text: "Loading persona…".to_string() } },
                        None => rsx! { EmptyState { title: "Select a persona.".to_string(), hint: None } },
                        Some(persona) => rsx! {
                            {
                                let accepted = persona.canon.clone();
                                let pending: Vec<CanonRevision> = revisions
                                    .iter()
                                    .filter(|revision| revision.state == CanonState::Proposed)
                                    .cloned()
                                    .collect();
                                let history: Vec<CanonRevision> = revisions
                                    .iter()
                                    .filter(|revision| revision.state != CanonState::Proposed)
                                    .cloned()
                                    .collect();
                                rsx! {
                                    div { class: "work-head",
                                        div { h1 { "{persona.name}" } p { class: "hint", "company {persona.company_id} · {persona.pending_proposals} pending proposal(s)" } }
                                        div { class: "spacer" }
                                        button { class: "btn btn-glass", onclick: move |_| wizard.set(Some((Some(persona.id), None))), "Edit" }
                                        if let Some(canon) = accepted.clone() {
                                            Chip { label: format!("Accepted rev {}", canon.revision), tone: Some("ok".to_string()) }
                                        }
                                    }
                                    Panel { title: "Accepted canon".to_string(), caption: Some("read-only".to_string()),
                                        if let Some(canon) = accepted.clone() {
                                            div { class: "glass-card",
                                                p { class: "op-desc", "{canon.body.voice}" }
                                                div { class: "row",
                                                    Chip { label: "accepted".to_string(), tone: Some("ok".to_string()) }
                                                    span { class: "chip", "source {canon.source}" }
                                                    span { class: "chip muted", "rev {canon.revision}" }
                                                    span { class: "label-tech", "by {canon.proposed_by}" }
                                                }
                                                if !canon.body.rules.is_empty() {
                                                    div { class: "facet",
                                                        span { class: "label-tech", "Rules" }
                                                        ul { class: "facet-list",
                                                            for rule in canon.body.rules.iter() { li { key: "{rule}", "{rule}" } }
                                                        }
                                                    }
                                                }
                                                if !canon.body.avoid.is_empty() {
                                                    div { class: "facet",
                                                        span { class: "label-tech", "Avoid" }
                                                        ul { class: "facet-list",
                                                            for rule in canon.body.avoid.iter() { li { key: "{rule}", "{rule}" } }
                                                        }
                                                    }
                                                }
                                                if !canon.body.world.is_empty() {
                                                    div { class: "facet",
                                                        span { class: "label-tech", "World" }
                                                        ul { class: "facet-list",
                                                            for rule in canon.body.world.iter() { li { key: "{rule}", "{rule}" } }
                                                        }
                                                    }
                                                }
                                            }
                                        } else {
                                            EmptyState { title: "No accepted canon yet.".to_string(), hint: Some("Accept a proposal or submit the editor below.".to_string()) }
                                        }
                                    }
                                    Panel { title: "Pending proposals".to_string(), caption: Some(format!("{}", pending.len())),
                                        if pending.is_empty() {
                                            EmptyState { title: "No pending proposals.".to_string(), hint: None }
                                        } else {
                                            div { class: "proposal-list",
                                                for revision in pending {
                                                    {
                                                        let client_accept = client.clone();
                                                        let client_reject = client.clone();
                                                        let accept_rev = revision.clone();
                                                        let reject_rev = revision.clone();
                                                        let editable = revision.body.voice.clone();
                                                        let description = describe_canon(&revision.body);
                                                        rsx! {
                                                            div {
                                                            if let Some((label, tone)) = canon_review_chip(&revision) { Chip { label: label.to_string(), tone: Some(tone.to_string()) } }
                                                            if revision.review_requested_at.is_none() {
                                                                {
                                                                    let client = client.clone();
                                                                    let (revision_id, persona_id) = (revision.id, revision.persona_id);
                                                                    rsx! { button { class: "btn btn-glass btn-xs", onclick: move |_| {
                                                                        let client = client.clone();
                                                                        spawn(async move {
                                                                            match api::timed(client.request_canon_comments(revision_id)).await {
                                                                                Ok(_) => store.notify(true, "Sent to the CEO for comments.".to_string()),
                                                                                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                            }
                                                                            api::load_canon(store, client, persona_id).await;
                                                                        });
                                                                    }, "Send for comments" } }
                                                                }
                                                            }
                                                            ProposalCard {
                                                                key: "{revision.id}",
                                                                title: format!("revision {} · {}", revision.revision, revision.source),
                                                                description,
                                                                reason: revision.rationale.clone(),
                                                                editable: Some(editable),
                                                                focused: (store.focus_proposal).cloned() == Some(revision.id),
                                                                on_accept: move |edited: Option<String>| {
                                                                    let decision = match edited {
                                                                        Some(voice) => {
                                                                            let mut body = accept_rev.body.clone();
                                                                            body.voice = voice;
                                                                            CanonDecisionIn::AcceptEdited { body }
                                                                        }
                                                                        None => CanonDecisionIn::Accept,
                                                                    };
                                                                     let client = client_accept.clone();
                                                                     let revision_id = accept_rev.id;
                                                                     let persona_id = accept_rev.persona_id;
                                                                     if canon_review_chip(&accept_rev) == Some(("Waiting for CEO comments", "warn")) {
                                                                         pending_accept.set(Some((revision_id, persona_id, decision)));
                                                                         return;
                                                                     }
                                                                    spawn(async move {
                                                                        match api::timed(client.decide_canon(revision_id, &decision)).await {
                                                                            Ok(revision) => {
                                                                                store.notify(true, format!("Canon revision {} is {}.", revision.revision, revision.state));
                                                                                api::load_canon(store, client.clone(), persona_id).await;
                                                                                api::refresh_base(store, client).await;
                                                                                form_for.set(None);
                                                                                store.set_focus_proposal(None);
                                                                            }
                                                                            Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                        }
                                                                    });
                                                                },
                                                                on_reject: move |reason: Option<String>| {
                                                                    let decision = CanonDecisionIn::Reject { reason };
                                                                    let client = client_reject.clone();
                                                                    let revision_id = reject_rev.id;
                                                                    let persona_id = reject_rev.persona_id;
                                                                    spawn(async move {
                                                                        match api::timed(client.decide_canon(revision_id, &decision)).await {
                                                                            Ok(revision) => {
                                                                                store.notify(true, format!("Canon revision {} is {}.", revision.revision, revision.state));
                                                                                api::load_canon(store, client.clone(), persona_id).await;
                                                                                api::refresh_base(store, client).await;
                                                                                form_for.set(None);
                                                                                store.set_focus_proposal(None);
                                                                            }
                                                                            Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                        }
                                                                    });
                                                                },
                                                            }
                                                            for comment in revision.comments.iter() {
                                                                div { class: "glass-card", p { class: "label-tech", "{comment.author} · {comment.author_role} · {comment.created_at}" } p { style: "white-space: pre-wrap", "{comment.body}" } }
                                                            }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Panel { title: "Canon history".to_string(), caption: Some("newest first".to_string()),
                                        if history.is_empty() {
                                            EmptyState { title: "No canon history yet.".to_string(), hint: None }
                                        } else {
                                            div { class: "delta-list",
                                                for revision in history {
                                                    {
                                                        let (state_label, _state_tone) = labels::canon_state(revision.state);
                                                        rsx! {
                                                            DeltaRow {
                                                                key: "{revision.id}",
                                                                seq: revision.revision.to_string(),
                                                                category: state_label.to_string(),
                                                                description: format!("{} · {} · {}", revision.source, revision.proposed_by, format::preview(&describe_canon(&revision.body), 90)),
                                                                action: None,
                                                                on_confirm: move |_| {},
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Panel { title: "Rules editor".to_string(), caption: Some("submits a proposal".to_string()),
                                        div { class: "form-grid",
                                            div { class: "field-row",
                                                label { class: "label-tech", r#for: "canon-voice", "Voice" }
                                                textarea {
                                                    id: "canon-voice", class: "input", value: "{voice}",
                                                    oninput: move |e| voice.set(e.value()),
                                                }
                                            }
                                            ListEditor {
                                                title: "Rules".to_string(),
                                                hint: "Hard constraints the copy must follow.".to_string(),
                                                items: rules,
                                                placeholder: "Add a rule…".to_string(),
                                            }
                                            ListEditor {
                                                title: "Avoid".to_string(),
                                                hint: "Words, claims and tones to avoid.".to_string(),
                                                items: avoid,
                                                placeholder: "Add something to avoid…".to_string(),
                                            }
                                            ListEditor {
                                                title: "World".to_string(),
                                                hint: "Accepted world facts the persona can rely on.".to_string(),
                                                items: world,
                                                placeholder: "Add a world fact…".to_string(),
                                            }
                                            div { class: "field-row",
                                                label { class: "label-tech", r#for: "canon-rationale", "Rationale" }
                                                input {
                                                    id: "canon-rationale", class: "input",
                                                    placeholder: "why this change",
                                                    value: "{rationale}", oninput: move |e| rationale.set(e.value()),
                                                }
                                            }
                                            div { class: "form-actions",
                                                button { class: "btn btn-primary", onclick: submit_proposal, "Submit proposal" }
                                            }
                                            p { class: "label-tech", "This never writes accepted canon directly; the proposal appears above for review." }
                                        }
                                    }
                                    Panel { title: "Creative chat".to_string(), caption: Some("persona-scoped".to_string()),
                                        {
                                            let lines = transcript(&(store.chat_persona).cloned().data.unwrap_or_default());
                                            rsx! {
                                                ChatPanel {
                                                    lines,
                                                    disabled: (store.selected_persona).cloned().is_none(),
                                                    placeholder: "ask the assistant to propose canon…".to_string(),
                                                    on_send: send_chat,
                                                    on_review: review_from_chat,
                                                }
                                            }
                                        }
                                        if let Some(error) = (store.chat_persona).cloned().error {
                                            ErrorCard { code: error.code, message: error.message }
                                        }
                                    }
                                }
                            }
                        },
                    }
                },
                false => rsx! {
                    if let Some(error) = save_error.cloned() { ErrorCard { code: error.code, message: error.message } }
                    match influencer {
                        None if influencers.data.is_none() && influencers.error.is_none() => rsx! { Loading { text: "Loading influencers…".to_string() } },
                        None => rsx! { EmptyState { title: "Select an influencer.".to_string(), hint: None } },
                        Some(item) => {
                            let (status, tone) = influencer_status(&item);
                            let client_save = client.clone();
                            let input_name = item.name.clone();
                            let kind = item.kind;
                            let id = item.id;
                            rsx! {
                                div { class: "work-head", h1 { "{item.name}" } Chip { label: status.to_string(), tone: Some(tone.to_string()) } button { class: "btn btn-glass", onclick: move |_| wizard.set(Some((None, Some(id)))), "New character" } }
                                Panel { title: "Influencer".to_string(), caption: None,
                                    div { class: "form-grid",
                                        div { class: "field-row", label { class: "label-tech", r#for: "influencer-premise", "Premise" } textarea { id: "influencer-premise", class: "input", value: "{premise}", oninput: move |e| premise.set(e.value()) } }
                                        div { class: "field-row", label { class: "label-tech", r#for: "influencer-dynamics", "Dynamics" } textarea { id: "influencer-dynamics", class: "input", value: "{dynamics}", oninput: move |e| dynamics.set(e.value()) } }
                                        div { class: "form-actions", button { class: "btn btn-primary", onclick: move |_| {
                                            let input = InfluencerIn { name: input_name.clone(), kind, premise: premise.cloned(), dynamics: dynamics.cloned() };
                                            let client = client_save.clone();
                                            spawn(async move { match api::timed(client.update_influencer(id, &input)).await {
                                                Ok(_) => { save_error.set(None); api::refresh_influencers(store, &client).await; },
                                                Err(error) => { if (store.selected_influencer).cloned() == Some(id) { save_error.set(Some(error)); } },
                                            } });
                                        }, "Save" } }
                                        p { "Accounts: {item.account_ids.join(\", \")}" }
                                    }
                                }
                                Panel { title: "Members".to_string(), caption: None,
                                    if item.members.is_empty() { EmptyState { title: "No members yet.".to_string(), hint: None } }
                                    for member in item.members {
                                        { let bio_done = personas.data.as_ref().and_then(|people| people.iter().find(|p| p.id == member.persona_id)).is_some_and(|p| super::character::bio_stage_done(&p.profile));
                                        rsx! {
                                        div { class: "glass-card", key: "{member.persona_id}",
                                            h3 { "{member.name}" }
                                            p { "{member.role}" }
                                            div { class: "row", Chip { label: status.to_string(), tone: Some(tone.to_string()) }
                                                Chip { label: if bio_done { "Bio · done".to_string() } else { "Bio · —".to_string() }, tone: Some(if bio_done { "ok" } else { "muted" }.to_string()) }
                                                for stage in ["Look", "3D", "Rig", "Voice"] { Chip { label: format!("{stage} · —"), tone: Some("muted".to_string()) } }
                                            }
                                        }
                                        } }
                                    }
                                }
                            }
                        },
                    }
                },
            }
        }
        if show_new() { NewInfluencerDialog { on_close: move |_| show_new.set(false) } }
        if let Some((revision_id, persona_id, decision)) = pending_accept() {
            GatePrompt { label: "The CEO has not commented yet. Accept anyway?".to_string(), approve_label: "Accept anyway".to_string(), reject_label: "Cancel".to_string(),
                on_reject: move |_| pending_accept.set(None),
                on_approve: { let client = client.clone(); move |_| { let client = client.clone(); let decision = decision.clone(); pending_accept.set(None); spawn(async move {
                    match api::timed(client.decide_canon(revision_id, &decision)).await {
                        Ok(revision) => { store.notify(true, format!("Canon revision {} is {}.", revision.revision, revision.state)); api::load_canon(store, client.clone(), persona_id).await; api::refresh_base(store, client).await; form_for.set(None); store.set_focus_proposal(None); },
                        Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                    }
                }); } },
            }
        }
        if let Some((persona_id, influencer_id)) = wizard() { super::character::CharacterWizard { persona_id, influencer_id, on_close: move |_| wizard.set(None) } }
    }
}

fn remaining_members(selected: &[(Uuid, String)], added: &[Uuid]) -> Vec<(Uuid, String)> {
    selected.iter().filter(|(id, _)| !added.contains(id)).cloned().collect()
}

fn solo_allows_one_member(kind: contracts::InfluencerKind, count: usize) -> bool {
    kind != contracts::InfluencerKind::Solo || count <= 1
}

#[component]
fn NewInfluencerDialog(on_close: EventHandler<()>) -> Element {
    use contracts::{InfluencerKind, InfluencerMemberIn};
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut name = use_signal(String::new);
    let mut kind = use_signal(|| InfluencerKind::Solo);
    let mut premise = use_signal(String::new);
    let mut dynamics = use_signal(String::new);
    let mut selected = use_signal(Vec::<(Uuid, String)>::new);
    let mut added = use_signal(Vec::<Uuid>::new);
    let mut created = use_signal(|| None::<Uuid>);
    let mut error = use_signal(|| None::<crate::state::UiError>);
    let mut busy = use_signal(|| false);
    let personas = (store.personas).cloned().data.unwrap_or_default();
    let locked = created().is_some() || busy();
    let valid = !name().trim().is_empty() && solo_allows_one_member(kind(), selected().len());
    rsx! {
        div { class: "influencer-dialog-backdrop",
            section { class: "influencer-dialog glass-card", role: "dialog", aria_modal: "true", aria_labelledby: "new-influencer-title",
                h2 { id: "new-influencer-title", "New influencer" }
                p { class: "hint", "One premise, one set of accounts, and one or more characters." }
                if let Some(err) = error() { ErrorCard { code: err.code, message: err.message } }
                div { class: "form-grid",
                    div { class: "field-row", label { class: "label-tech", r#for: "new-influencer-name", "Name" }
                        input { id: "new-influencer-name", class: "input", required: true, disabled: locked, value: "{name}", oninput: move |e| name.set(e.value()) }
                    }
                    fieldset { disabled: locked, legend { class: "label-tech", "Format" }
                        label { input { r#type: "radio", name: "influencer-kind", checked: kind() == InfluencerKind::Solo, onchange: move |_| kind.set(InfluencerKind::Solo) } " Solo" }
                        label { input { r#type: "radio", name: "influencer-kind", checked: kind() == InfluencerKind::Group, onchange: move |_| kind.set(InfluencerKind::Group) } " Group" }
                    }
                    div { class: "field-row", label { class: "label-tech", r#for: "new-influencer-premise", "Premise" }
                        textarea { id: "new-influencer-premise", class: "input", disabled: locked, value: "{premise}", oninput: move |e| premise.set(e.value()) }
                    }
                    div { class: "field-row", label { class: "label-tech", r#for: "new-influencer-dynamics", "How they play together" }
                        textarea { id: "new-influencer-dynamics", class: "input", disabled: locked, value: "{dynamics}", oninput: move |e| dynamics.set(e.value()) }
                    }
                    fieldset { disabled: busy(), legend { class: "label-tech", "Members" }
                        for persona in personas {
                            {
                                let id = persona.id;
                                let member = selected().iter().find(|(member, _)| *member == id).cloned();
                                let checked = member.is_some();
                                let role = member.map(|(_, role)| role).unwrap_or_default();
                                rsx! { div { class: "row", key: "{id}",
                                    label { input { r#type: "checkbox", checked, disabled: added().contains(&id) || (!checked && !solo_allows_one_member(kind(), selected().len() + 1)), onchange: move |e| {
                                        if e.checked() { selected.write().push((id, String::new())); } else { selected.write().retain(|(member, _)| *member != id); }
                                    } } " {persona.name}" }
                                    input { class: "input", placeholder: "Role", aria_label: "Role for {persona.name}", disabled: !checked || added().contains(&id), value: "{role}", oninput: move |e| {
                                        if let Some((_, role)) = selected.write().iter_mut().find(|(member, _)| *member == id) { *role = e.value(); }
                                    } }
                                } }
                            }
                        }
                    }
                    if !solo_allows_one_member(kind(), selected().len()) { p { class: "notice err", "Solo allows at most one member." } }
                    div { class: "form-actions",
                        button { class: "btn btn-glass", disabled: busy(), onclick: move |_| on_close.call(()), "Cancel" }
                        button { class: "btn btn-primary", disabled: busy() || !valid, onclick: move |_| {
                            if busy() || !valid { return; }
                            busy.set(true);
                            error.set(None);
                            let client = client.clone();
                            let input = InfluencerIn { name: name().trim().to_string(), kind: kind(), premise: premise(), dynamics: dynamics() };
                            let members = remaining_members(&selected(), &added());
                            spawn(async move {
                                let id = if let Some(id) = created() { id } else {
                                    match api::timed(client.create_influencer(&input)).await {
                                        Ok(influencer) => { created.set(Some(influencer.id)); influencer.id },
                                        Err(err) => { error.set(Some(err)); busy.set(false); return; }
                                    }
                                };
                                for (persona, role) in members {
                                    if let Err(err) = api::timed(client.put_influencer_member(id, persona, &InfluencerMemberIn { role })).await {
                                        error.set(Some(err));
                                        api::refresh_influencers(store, &client).await;
                                        busy.set(false);
                                        return;
                                    }
                                    added.write().push(persona);
                                }
                                api::refresh_influencers(store, &client).await;
                                let mut selection = store.selected_influencer;
                                selection.set(Some(id));
                                let mut show_character = store.influencers_show_character;
                                show_character.set(false);
                                on_close.call(());
                            });
                        }, if created().is_some() { "Retry" } else { "Create influencer" } }
                    }
                }
            }
        }
    }
}

fn describe_canon(body: &CanonBody) -> String {
    format!("{} · {} rules · {} avoid · {} world", body.voice, body.rules.len(), body.avoid.len(), body.world.len())
}

fn canon_review_chip(revision: &CanonRevision) -> Option<(&'static str, &'static str)> {
    if revision.state != CanonState::Proposed || revision.review_requested_at.is_none() {
        None
    } else if revision.comments.iter().any(|c| c.author_role == contracts::Role::Agent) {
        Some(("CEO commented", "ok"))
    } else {
        Some(("Waiting for CEO comments", "warn"))
    }
}

fn transcript(messages: &[contracts::ChatMessage]) -> Vec<TranscriptLine> {
    messages
        .iter()
        .map(|message| TranscriptLine {
            role: message.role.clone(),
            text: message.body.clone(),
            proposal: message.proposal_id.map(|_| "Review canon proposal".to_string()),
        })
        .collect()
}

fn influencer_status(influencer: &contracts::Influencer) -> (&'static str, &'static str) {
    if influencer.members.is_empty() {
        ("Empty", "muted")
    } else if influencer.members.iter().any(|member| !member.has_accepted_canon) {
        ("Draft", "warn")
    } else {
        ("Ready", "ok")
    }
}

#[cfg(test)]
mod tests {
    use contracts::{Influencer, InfluencerKind, InfluencerMember};
    use uuid::Uuid;

    #[test]
    fn review_chip_waits_until_an_agent_comments() {
        use contracts::{CanonBody, CanonComment, CanonRevision, CanonSource, CanonState, Role};
        let mut rev = CanonRevision {
            id: Uuid::new_v4(),
            persona_id: Uuid::new_v4(),
            revision: 1,
            base_revision: None,
            body: CanonBody::default(),
            state: CanonState::Proposed,
            source: CanonSource::Operator,
            rationale: None,
            proposed_by: String::new(),
            decided_by: None,
            created_at: chrono::Utc::now(),
            review_requested_at: None,
            review_issue_url: None,
            comments: vec![],
        };
        assert_eq!(super::canon_review_chip(&rev), None);
        rev.review_requested_at = Some(chrono::Utc::now());
        assert_eq!(super::canon_review_chip(&rev), Some(("Waiting for CEO comments", "warn")));
        rev.comments.push(CanonComment {
            id: Uuid::new_v4(),
            revision_id: rev.id,
            author: String::new(),
            author_role: Role::Operator,
            body: String::new(),
            created_at: chrono::Utc::now(),
        });
        assert_eq!(super::canon_review_chip(&rev), Some(("Waiting for CEO comments", "warn")));
        rev.comments[0].author_role = Role::Agent;
        assert_eq!(super::canon_review_chip(&rev), Some(("CEO commented", "ok")));
        rev.state = CanonState::Accepted;
        assert_eq!(super::canon_review_chip(&rev), None);
    }

    #[test]
    fn member_retry_skips_members_already_added() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(super::remaining_members(&[(a, "lead".into()), (b, "guest".into())], &[a]), vec![(b, "guest".into())]);
    }

    #[test]
    fn solo_allows_one_member() {
        assert!(super::solo_allows_one_member(InfluencerKind::Solo, 1));
        assert!(!super::solo_allows_one_member(InfluencerKind::Solo, 2));
        assert!(super::solo_allows_one_member(InfluencerKind::Group, 2));
    }

    #[test]
    fn influencer_status_is_draft_until_every_member_has_accepted_canon() {
        let mut influencer = Influencer {
            id: Uuid::new_v4(),
            company_id: "company".into(),
            name: "Test".into(),
            kind: InfluencerKind::Group,
            premise: String::new(),
            dynamics: String::new(),
            members: Vec::new(),
            account_ids: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(super::influencer_status(&influencer), ("Empty", "muted"));
        influencer.members.push(InfluencerMember {
            persona_id: Uuid::new_v4(),
            name: "One".into(),
            role: String::new(),
            has_accepted_canon: false,
        });
        assert_eq!(super::influencer_status(&influencer), ("Draft", "warn"));
        influencer.members[0].has_accepted_canon = true;
        assert_eq!(super::influencer_status(&influencer), ("Ready", "ok"));
        influencer.members.push(InfluencerMember {
            persona_id: Uuid::new_v4(),
            name: "Two".into(),
            role: String::new(),
            has_accepted_canon: false,
        });
        assert_eq!(super::influencer_status(&influencer), ("Draft", "warn"));
        influencer.members[1].has_accepted_canon = true;
        assert_eq!(super::influencer_status(&influencer), ("Ready", "ok"));
    }
}
