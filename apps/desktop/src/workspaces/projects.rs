//! Projects, individuals and proposal-only canon review.
//!
//! Canon is proposal-only: the rules editor submits `CanonProposalIn`; accepting
//! a proposal is the only path that changes accepted canon.

use contracts::{
    CanonBody, CanonDecisionIn, CanonProposalIn, CanonRevision, CanonSource, CanonState, ChatMessageIn, ProjectIn, client::HubClient,
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
pub fn ProjectsView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut show_individual = store.projects_show_individual;
    let mut show_new = use_signal(|| false);
    let mut wizard = use_signal(|| None::<(Option<Uuid>, Option<Uuid>)>);
    let mut pending_accept = use_signal(|| None::<(Uuid, Uuid, CanonDecisionIn)>);
    let projects = (store.projects).cloned();
    let selected_project_id = (store.selected_project).cloned();
    let project = projects.data.as_ref().and_then(|items| items.iter().find(|item| Some(item.id) == selected_project_id).cloned());
    let mut premise = use_signal(String::new);
    let mut dynamics = use_signal(String::new);
    let mut models = use_signal(contracts::ProjectModels::default);
    let mut project_form_for = use_signal(|| Option::<Uuid>::None);
    let mut save_error = use_signal(|| Option::<crate::state::UiError>::None);
    use_effect(move || {
        let id = (store.selected_project).cloned();
        if project_form_for.cloned() == id {
            return;
        }
        if let Some(item) = (store.projects).cloned().data.and_then(|items| items.into_iter().find(|item| Some(item.id) == id)) {
            premise.set(item.premise);
            dynamics.set(item.dynamics);
            models.set(item.models);
            project_form_for.set(id);
            save_error.set(None);
        }
    });
    let personas = (store.personas).cloned();
    let selected_persona_id = (store.selected_persona).cloned();
    let persona = personas.data.as_ref().and_then(|items| items.iter().find(|p| Some(p.id) == selected_persona_id).cloned());
    let member_client = client.clone();
    use_effect(move || {
        let selected = (store.selected_project).cloned();
        let members = (store.projects)
            .cloned()
            .data
            .unwrap_or_default()
            .into_iter()
            .find(|item| Some(item.id) == selected)
            .map(|item| item.members)
            .unwrap_or_default();
        let client = member_client.clone();
        spawn(async move {
            for member in members {
                let Some(reference_id) = member.reference_asset_id else { continue };
                if store.asset_images.peek().contains_key(&reference_id) {
                    continue;
                }
                if let Ok(look) = api::timed(client.individual_look(member.persona_id)).await
                    && let Some(asset) = look
                        .uploads
                        .into_iter()
                        .chain(look.passes.into_iter().flat_map(|pass| pass.assets))
                        .find(|asset| asset.id == reference_id)
                {
                    api::load_asset_bytes(store, client.clone(), &[asset]).await;
                }
            }
        });
    });
    let mut reference = use_signal(|| None::<contracts::Asset>);
    let reference_client = client.clone();
    use_effect(move || {
        let selected = (store.selected_persona).cloned();
        if wizard().is_some() {
            return;
        }
        reference.set(None);
        let Some(persona) = selected else { return };
        let client = reference_client.clone();
        spawn(async move {
            match api::timed(client.individual_look(persona)).await {
                Ok(look) => {
                    let asset = look.reference.and_then(|reference| {
                        look.uploads
                            .into_iter()
                            .chain(look.passes.into_iter().flat_map(|pass| pass.assets))
                            .find(|asset| asset.id == reference.asset_id)
                    });
                    if let Some(asset) = asset.as_ref() {
                        api::load_asset_bytes(store, client, std::slice::from_ref(asset)).await;
                    }
                    if (store.selected_persona).cloned() == Some(persona) {
                        reference.set(asset);
                    }
                }
                Err(err) => store.notify(false, format!("{} — {}", err.code, err.message)),
            }
        });
    });
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
        aside { class: "context", aria_label: "Projects and individuals", style: "overflow-wrap: anywhere;",
            div { class: "context-head",
                h2 { "Directory" }
            }
            div { class: "row", style: "padding: 0 4px 10px;",
                button { class: "btn secondary btn-xs", onclick: move |_| show_new.set(true), "New project" }
                button { class: "btn secondary btn-xs", onclick: move |_| wizard.set(Some((None, None))), "Create individual" }
            }
            div { class: "context-list",
                match projects.data.clone() {
                    Some(items) if items.is_empty() => rsx! { p { class: "ctx-empty", "No projects yet." } },
                    Some(items) => rsx! {
                        for item in items {
                            {
                                let (status, _) = project_status(&item);
                                let kind = match item.kind { contracts::ProjectKind::Solo => "Solo", contracts::ProjectKind::Group => "Group" };
                                let members = item.members.len();
                                let meta = match members {
                                    0 => format!("{kind} · {status}"),
                                    1 => format!("{kind} · 1 individual · {status}"),
                                    _ => format!("{kind} · {members} individuals · {status}"),
                                };
                                rsx! { ContextItem {
                                    key: "{item.id}", title: item.name.clone(),
                                    meta,
                                    active: !show_individual.cloned() && Some(item.id) == selected_project_id,
                                    on_select: move |_| { let mut selected = store.selected_project; selected.set(Some(item.id)); show_individual.set(false); },
                                } }
                            }
                        }
                    },
                    None if projects.error.is_none() => rsx! { p { class: "ctx-empty", "Loading projects…" } },
                    None => rsx! {},
                }
                h2 { "Individuals" }
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
                                active: show_individual.cloned() && Some(item.id) == selected_persona_id,
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
            if let Some(error) = projects.error.clone() { ErrorCard { code: error.code, message: error.message } }
            if let Some(error) = personas.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match show_individual.cloned() {
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
                                        div { h1 { "{persona.name}" } }
                                        div { class: "spacer" }
                                        button { class: "btn secondary", onclick: move |_| wizard.set(Some((Some(persona.id), None))), "Edit individual" }
                                    }
                                    Panel { title: "Pinned reference".to_string(), caption: None,
                                        if let Some(asset) = reference() {
                                            div { class: "asset-frame", if let Some(uri) = (store.asset_images).cloned().get(&asset.id) { img { src: "{uri}", alt: "{asset.alt_text}" } } else { span { class: "asset-missing", "{asset.mime}" } } }
                                        } else { p { "No reference pinned yet." } }
                                        p { "Pinning another image asks first, lists the projects it affects, and marks 3D and rig for redo. The approved model stays in use until you approve a new one." }
                                    }
                                    section { class: "panel", aria_labelledby: "accepted-canon-title",
                                        div { class: "panel-head",
                                            h3 { id: "accepted-canon-title", "Accepted canon" }
                                            if let Some(canon) = accepted.as_ref() {
                                                Chip { label: format!("Revision {}", canon.revision), tone: Some("ok".to_string()) }
                                            }
                                        }
                                        if let Some(canon) = accepted.clone() {
                                            div { class: "glass-card",
                                                p { class: "op-desc", "{canon.body.voice}" }
                                                div { class: "row",
                                                    span { class: "chip", "source {canon.source}" }
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
                                    section { class: "panel", aria_labelledby: "pending-canon-title", style: "border-top: 1px solid var(--line-subtle); padding-top: 12px;",
                                        div { class: "panel-head",
                                            h3 { id: "pending-canon-title", "Pending proposals" }
                                            span { class: "badge-count", "{pending.len()}" }
                                        }
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
                    match project {
                        None if projects.data.is_none() && projects.error.is_none() => rsx! { Loading { text: "Loading projects…".to_string() } },
                        None => rsx! { EmptyState { title: "Select a project.".to_string(), hint: None } },
                        Some(item) => {
                            let (status, tone) = project_status(&item);
                            let client_save = client.clone();
                            let client_models = client.clone();
                            let input_name = item.name.clone();
                            let models_name = item.name.clone();
                            let kind = item.kind;
                            let id = item.id;
                            rsx! {
                                div { class: "work-head",
                                    div { h1 { "{item.name}" } }
                                    Chip { label: status.to_string(), tone: Some(tone.to_string()) }
                                }
                                section { class: "panel glass-card content-bounded",
                                    h2 { style: "font-size: 15px;", "Overview" }
                                    div { class: "form-stack content-bounded", style: "max-width: 720px;",
                                        div { class: "form-field",
                                            label { class: "label-tech", r#for: "project-premise", "Premise" }
                                            textarea { id: "project-premise", class: "input", value: "{premise}", oninput: move |e| premise.set(e.value()) }
                                        }
                                        div { class: "form-field",
                                            label { class: "label-tech", r#for: "project-dynamics", "Dynamics" }
                                            textarea { id: "project-dynamics", class: "input", value: "{dynamics}", oninput: move |e| dynamics.set(e.value()) }
                                        }
                                        div { class: "row", button { class: "btn primary", onclick: move |_| {
                                            let input = ProjectIn { name: input_name.clone(), kind, premise: premise.cloned(), dynamics: dynamics.cloned(), models: None };
                                            let client = client_save.clone();
                                            spawn(async move { match api::timed(client.update_project(id, &input)).await {
                                                Ok(_) => { save_error.set(None); api::refresh_projects(store, &client).await; },
                                                Err(error) => { if (store.selected_project).cloned() == Some(id) { save_error.set(Some(error)); } },
                                            } });
                                        }, "Save overview" } }
                                    }
                                }
                                section { class: "panel glass-card content-bounded",
                                    div { class: "row",
                                        h2 { style: "font-size: 15px;", "Members & Accounts" }
                                        div { class: "spacer" }
                                        button { class: "btn secondary", onclick: move |_| wizard.set(Some((None, Some(id)))), "Create project member" }
                                    }
                                    if item.members.is_empty() {
                                        p { class: "form-help", "No members yet." }
                                    } else {
                                        for member in item.members {
                                            {
                                                let bio_done = personas.data.as_ref().and_then(|people| people.iter().find(|p| p.id == member.persona_id)).is_some_and(|p| super::individual::bio_stage_done(&p.profile));
                                                rsx! {
                                                    div { class: "glass-card", key: "{member.persona_id}",
                                                        h3 { "{member.name}" }
                                                        p { "{member.role}" }
                                                        if let Some(reference_id) = member.reference_asset_id {
                                                            div { class: "asset-frame", if let Some(uri) = (store.asset_images).cloned().get(&reference_id) { img { src: "{uri}", alt: "Pinned reference for {member.name}" } } else { span { class: "asset-missing", "Reference image unavailable" } } }
                                                        }
                                                        div { class: "row", Chip { label: status.to_string(), tone: Some(tone.to_string()) }
                                                            Chip { label: if bio_done { "Bio · done".to_string() } else { "Bio · —".to_string() }, tone: Some(if bio_done { "ok" } else { "muted" }.to_string()) }
                                                            Chip { label: if member.reference_asset_id.is_some() { "Look · done".to_string() } else { "Look · —".to_string() }, tone: Some(if member.reference_asset_id.is_some() { "ok" } else { "muted" }.to_string()) }
                                                            for stage in ["3D", "Rig", "Voice"] { Chip { label: format!("{stage} · —"), tone: Some("muted".to_string()) } }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        p { class: "form-help", "Pinning another image asks first, lists the projects it affects, and marks 3D and rig for redo. The approved model stays in use until you approve a new one." }
                                    }
                                    section { class: "stack", style: "border-top: 1px solid var(--line-subtle); padding-top: 10px;",
                                        h3 { style: "font-size: 13px;", "Linked accounts" }
                                        if item.account_ids.is_empty() {
                                            p { "No linked accounts" }
                                            p { class: "form-help", "Ask a Hub operator to link a publishing account through the project API." }
                                        } else {
                                            p { class: "form-help", "{item.account_ids.join(\", \")}" }
                                        }
                                    }
                                }
                                section { class: "panel glass-card content-bounded",
                                    h2 { style: "font-size: 15px;", "Model configuration" }
                                    div { class: "form-stack",
                                        div { class: "form-grid-2",
                                        for (role, label) in [("checker", "Checker"), ("image", "Image"), ("assistant", "Assistant"), ("three_d", "3D"), ("voice", "Voice")] {
                                            {
                                                let selected = match role {
                                                    "checker" => models().checker.id,
                                                    "image" => models().image,
                                                    "assistant" => models().assistant,
                                                    "three_d" => models().three_d,
                                                    _ => models().voice,
                                                };
                                                rsx! {
                                                    div { class: "form-field", key: "{role}",
                                                        label { class: "label-tech", r#for: "model-{role}", "{label}" }
                                                        select { id: "model-{role}", class: "input", value: "{selected}", onchange: move |e| {
                                                            let value = e.value();
                                                            let mut current = models.write();
                                                            match role {
                                                                "checker" => {
                                                                    current.checker.id = value;
                                                                    let efforts = model_options("checker").into_iter().find(|(id, _)| *id == current.checker.id).map(|(_, efforts)| efforts).unwrap_or_default();
                                                                    if current.checker.effort.as_deref().is_some_and(|effort| !efforts.contains(&effort)) {
                                                                        current.checker.effort = None;
                                                                    }
                                                                },
                                                                "image" => current.image = value,
                                                                "assistant" => current.assistant = value,
                                                                "three_d" => current.three_d = value,
                                                                _ => current.voice = value,
                                                            }
                                                        },
                                                            for (id, _) in model_options(role) { option { value: "{id}", "{id}" } }
                                                        }
                                                    }
                                                    if role == "checker" {
                                                        div { class: "form-field",
                                                            label { class: "label-tech", r#for: "model-checker-effort", "Checker effort" }
                                                            select { id: "model-checker-effort", class: "input", aria_describedby: "model-checker-help",
                                                                value: models().checker.effort.unwrap_or_default(), onchange: move |e| {
                                                                    let value = e.value();
                                                                    models.write().checker.effort = (!value.is_empty()).then_some(value);
                                                                },
                                                                option { value: "", "Default" }
                                                                for effort in model_options("checker").into_iter().find(|(id, _)| *id == models().checker.id).map(|(_, efforts)| efforts).unwrap_or_default().iter() {
                                                                    option { value: "{effort}", "{effort}" }
                                                                }
                                                            }
                                                            p { class: "form-help", id: "model-checker-help", "Controls automated image checks. Verify the results before keeping an image." }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        }
                                        div { class: "row", button { class: "btn primary", onclick: move |_| {
                                            let input = ProjectIn { name: models_name.clone(), kind, premise: premise.cloned(), dynamics: dynamics.cloned(), models: Some(models.cloned()) };
                                            let client = client_models.clone();
                                            spawn(async move { match api::timed(client.update_project(id, &input)).await {
                                                Ok(project) => {
                                                    if (store.selected_project).cloned() == Some(id) { save_error.set(None); models.set(project.models); }
                                                    api::refresh_projects(store, &client).await;
                                                },
                                                Err(error) => { if (store.selected_project).cloned() == Some(id) { save_error.set(Some(error)); } },
                                            } });
                                        }, "Save models" } }
                                    }
                                }
                            }
                        },
                    }
                },
            }
        }
        if show_new() { NewProjectDialog { on_close: move |_| show_new.set(false) } }
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
        if let Some((persona_id, project_id)) = wizard() { super::individual::IndividualWizard { persona_id, project_id, on_close: move |_| wizard.set(None) } }
    }
}

fn model_options(role: &str) -> Vec<(&'static str, &'static [&'static str])> {
    contracts::MODEL_ROUTES.iter().filter(|(candidate, _, _)| *candidate == role).map(|(_, id, efforts)| (*id, *efforts)).collect()
}

fn remaining_members(selected: &[(Uuid, String)], added: &[Uuid]) -> Vec<(Uuid, String)> {
    selected.iter().filter(|(id, _)| !added.contains(id)).cloned().collect()
}

fn solo_allows_one_member(kind: contracts::ProjectKind, count: usize) -> bool {
    kind != contracts::ProjectKind::Solo || count <= 1
}

#[component]
fn NewProjectDialog(on_close: EventHandler<()>) -> Element {
    use contracts::{ProjectKind, ProjectMemberIn};
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut name = use_signal(String::new);
    let mut kind = use_signal(|| ProjectKind::Solo);
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
        div { class: "project-dialog-backdrop",
            section { class: "project-dialog glass-card", role: "dialog", aria_modal: "true", aria_labelledby: "new-project-title",
                h2 { id: "new-project-title", "New project" }
                p { class: "hint", "One premise, one set of accounts, and one or more individuals." }
                if let Some(err) = error() { ErrorCard { code: err.code, message: err.message } }
                div { class: "form-grid",
                    div { class: "field-row", label { class: "label-tech", r#for: "new-project-name", "Name" }
                        input { id: "new-project-name", class: "input", required: true, disabled: locked, value: "{name}", oninput: move |e| name.set(e.value()) }
                    }
                    fieldset { disabled: locked, legend { class: "label-tech", "Format" }
                        label { input { r#type: "radio", name: "project-kind", checked: kind() == ProjectKind::Solo, onchange: move |_| kind.set(ProjectKind::Solo) } " Solo" }
                        label { input { r#type: "radio", name: "project-kind", checked: kind() == ProjectKind::Group, onchange: move |_| kind.set(ProjectKind::Group) } " Group" }
                    }
                    div { class: "field-row", label { class: "label-tech", r#for: "new-project-premise", "Premise" }
                        textarea { id: "new-project-premise", class: "input", disabled: locked, value: "{premise}", oninput: move |e| premise.set(e.value()) }
                    }
                    div { class: "field-row", label { class: "label-tech", r#for: "new-project-dynamics", "How they play together" }
                        textarea { id: "new-project-dynamics", class: "input", disabled: locked, value: "{dynamics}", oninput: move |e| dynamics.set(e.value()) }
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
                            let input = ProjectIn { name: name().trim().to_string(), kind: kind(), premise: premise(), dynamics: dynamics(), models: None };
                            let members = remaining_members(&selected(), &added());
                            spawn(async move {
                                let id = if let Some(id) = created() { id } else {
                                    match api::timed(client.create_project(&input)).await {
                                        Ok(project) => { created.set(Some(project.id)); project.id },
                                        Err(err) => { error.set(Some(err)); busy.set(false); return; }
                                    }
                                };
                                for (persona, role) in members {
                                    if let Err(err) = api::timed(client.put_project_member(id, persona, &ProjectMemberIn { role })).await {
                                        error.set(Some(err));
                                        api::refresh_projects(store, &client).await;
                                        busy.set(false);
                                        return;
                                    }
                                    added.write().push(persona);
                                }
                                api::refresh_projects(store, &client).await;
                                let mut selection = store.selected_project;
                                selection.set(Some(id));
                                let mut show_individual = store.projects_show_individual;
                                show_individual.set(false);
                                on_close.call(());
                            });
                        }, if created().is_some() { "Retry" } else { "Create project" } }
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

fn project_status(project: &contracts::Project) -> (&'static str, &'static str) {
    if project.members.is_empty() {
        ("No members", "muted")
    } else if project.members.iter().any(|member| !member.has_accepted_canon) {
        ("Draft", "warn")
    } else {
        ("Ready", "ok")
    }
}

#[cfg(test)]
mod tests {
    use contracts::{Project, ProjectKind, ProjectMember};
    use uuid::Uuid;

    #[test]
    fn model_options_come_from_the_catalog() {
        assert_eq!(super::model_options("checker"), vec![("openai/gpt-6.1-sol", &["low", "medium", "high", "xhigh"][..])]);
    }

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
        assert!(super::solo_allows_one_member(ProjectKind::Solo, 1));
        assert!(!super::solo_allows_one_member(ProjectKind::Solo, 2));
        assert!(super::solo_allows_one_member(ProjectKind::Group, 2));
    }

    #[test]
    fn project_status_is_draft_until_every_member_has_accepted_canon() {
        let mut project = Project {
            id: Uuid::new_v4(),
            company_id: "company".into(),
            name: "Test".into(),
            kind: ProjectKind::Group,
            premise: "An established premise".into(),
            dynamics: String::new(),
            models: Default::default(),
            members: Vec::new(),
            account_ids: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(super::project_status(&project), ("No members", "muted"));
        project.members.push(ProjectMember {
            persona_id: Uuid::new_v4(),
            name: "One".into(),
            role: String::new(),
            has_accepted_canon: false,
            reference_asset_id: None,
        });
        assert_eq!(super::project_status(&project), ("Draft", "warn"));
        project.members[0].has_accepted_canon = true;
        assert_eq!(super::project_status(&project), ("Ready", "ok"));
        project.members.push(ProjectMember {
            persona_id: Uuid::new_v4(),
            name: "Two".into(),
            role: String::new(),
            has_accepted_canon: false,
            reference_asset_id: None,
        });
        assert_eq!(super::project_status(&project), ("Draft", "warn"));
        project.members[1].has_accepted_canon = true;
        assert_eq!(super::project_status(&project), ("Ready", "ok"));
    }
}
