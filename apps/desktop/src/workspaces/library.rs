//! Library: personas + canon review, and the asset library.
//!
//! Canon is proposal-only: the rules editor submits `CanonProposalIn`; accepting
//! a proposal is the only path that changes accepted canon.

use contracts::{CanonBody, CanonDecisionIn, CanonProposalIn, CanonRevision, CanonSource, CanonState, ChatMessageIn, client::HubClient};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    actions, api,
    components::{
        Chip, EmptyState, ErrorCard, Loading, Panel,
        chat::{ChatPanel, TranscriptLine},
        proposal::{DeltaRow, ListEditor, ProposalCard},
    },
    format, labels,
    shell::ContextItem,
    state::{Loaded, Store},
};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tab {
    Personas,
    Assets,
}

#[component]
pub fn LibraryView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut tab = use_signal(|| Tab::Personas);
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

    let assets_client = client.clone();
    use_effect(move || {
        if tab.cloned() != Tab::Assets {
            return;
        }
        let Some(assets) = (store.assets).cloned().data else { return };
        let client = assets_client.clone();
        spawn(async move {
            api::load_asset_bytes(store, client, &assets).await;
        });
    });

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
        aside { class: "context", aria_label: "Personas",
            div { class: "context-head",
                h2 { "Library" }
                span { class: "label-tech", "personas" }
            }
            div { class: "context-list",
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
                                active: Some(item.id) == selected_persona_id,
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
                    h1 { "Library" }
                    p { class: "hint", "Persona canon is proposal-only: the assistant proposes, the operator accepts." }
                }
                div { class: "spacer" }
                div { class: "row",
                    button {
                        class: if tab.cloned() == Tab::Personas { "btn btn-primary" } else { "btn btn-glass" },
                        onclick: move |_| tab.set(Tab::Personas),
                        "Personas"
                    }
                    button {
                        class: if tab.cloned() == Tab::Assets { "btn btn-primary" } else { "btn btn-glass" },
                        onclick: move |_| tab.set(Tab::Assets),
                        "Assets"
                    }
                }
            }
            if let Some(error) = personas.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match tab.cloned() {
                Tab::Personas => rsx! {
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
                Tab::Assets => rsx! {
                    if let Some(error) = (store.assets).cloned().error {
                        ErrorCard { code: error.code, message: error.message }
                    }
                    match (store.assets).cloned().data.clone() {
                        None if (store.assets).cloned().error.is_none() => rsx! { Loading { text: "Loading assets…".to_string() } },
                        None => rsx! {},
                        Some(assets) if assets.is_empty() => rsx! {
                            EmptyState { title: "No assets yet.".to_string(), hint: Some("Assets arrive with successful generations.".to_string()) }
                        },
                        Some(assets) => rsx! {
                            if let Some(error) = (store.assets).cloned().error.clone() {
                                ErrorCard { code: error.code, message: error.message }
                            }
                            div { class: "asset-grid four",
                                for asset in assets {
                                    {
                                        let (state_label, state_tone) = labels::asset_state(asset.state);
                                        let image = (store.asset_images).cloned().get(&asset.id).cloned();
                                        let content_id = asset.content_item_id;
                                        rsx! {
                                            div { class: "glass-card asset-tile", key: "{asset.id}",
                                                div { class: "asset-frame",
                                                    if let Some(uri) = image {
                                                        img { src: "{uri}", alt: "{asset.alt_text}" }
                                                    } else {
                                                        span { class: "asset-missing", "{state_label} · {asset.mime}" }
                                                    }
                                                }
                                                div { class: "asset-meta",
                                                    Chip { label: state_label.to_string(), tone: Some(state_tone.to_string()) }
                                                    span { class: "chip muted", "sha {format::short_hash(&asset.sha256)}" }
                                                    if asset.selected {
                                                        span { class: "chip sage", "selected" }
                                                    }
                                                }
                                                div { class: "label-tech", "{asset.alt_text}" }
                                                button {
                                                    class: "btn btn-glass btn-xs",
                                                    onclick: move |_| actions::open_subject(store, "content", content_id),
                                                    "Open content"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    }
                },
            }
        }
    }
}

fn describe_canon(body: &CanonBody) -> String {
    format!("{} · {} rules · {} avoid · {} world", body.voice, body.rules.len(), body.avoid.len(), body.world.len())
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
