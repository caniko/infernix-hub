//! Studio: content briefs, generation, asset selection, release freeze,
//! review, scheduling and delivery intents — all through `HubClient`.

use std::collections::HashMap;

use chrono::{Duration as ChronoDuration, Utc};
use contracts::client::HubClient;
use contracts::{
    CancelOut, ChatMessageIn, FreezeReleaseIn, GenerateIn, LinkApprovalIn, ReleaseState, ScheduleIn, SelectAssetIn, TimingScope,
};
use dioxus::prelude::*;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    api,
    components::{
        Chip, EmptyState, ErrorCard, Loading, Panel,
        chat::{ChatPanel, TranscriptLine},
        proposal::DeltaRow,
    },
    format, labels, platform,
    shell::ContextItem,
    state::{Loaded, Store, Workspace},
};

fn generation_prompt(inputs: &str) -> Option<String> {
    let value: Value = serde_json::from_str(inputs).ok()?;
    value.get("prompt")?.as_str().map(str::to_owned)
}

fn set_generation_prompt(inputs: &str, prompt: String) -> Option<String> {
    let mut value: Value = serde_json::from_str(inputs).ok()?;
    *value.get_mut("prompt").filter(|value| value.is_string())? = Value::String(prompt);
    serde_json::to_string_pretty(&value).ok()
}

fn workflow_label(name: &str) -> &str {
    match name {
        "moss-look-img2img" => "Moss look · image to image",
        "moss-look-img2img-final" => "Moss look · final image to image",
        "moss-look-txt2img" => "Moss look · text to image",
        "poster-txt2img" => "Poster · text to image",
        "swatch" => "Swatch",
        _ => name,
    }
}

#[component]
pub fn StudioView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let content = (store.content).cloned();
    let content_count = content.data.as_ref().map_or(0, Vec::len);
    let detail = (store.content_detail).cloned();
    let selected_id = (store.selected_content).cloned();
    let detail_error = detail.error.clone();
    let selected = detail.data.as_ref().filter(|d| Some(d.item.id) == selected_id).cloned();

    let mut workflow_id = use_signal(|| Option::<Uuid>::None);
    let mut variants = use_signal(|| "4".to_string());
    let mut gen_inputs = use_signal(HashMap::<Uuid, String>::new);
    let mut four_up = use_signal(|| false);
    let mut alt = use_signal(HashMap::<Uuid, String>::new);
    let mut release_text = use_signal(String::new);
    let mut account_ids = use_signal(Vec::<String>::new);
    let mut not_before = use_signal(String::new);
    let mut not_after = use_signal(String::new);
    let mut schedule_at = use_signal(String::new);
    let mut settings = use_signal(platform::SettingsDraft::default);
    let mut settings_base = use_signal(serde_json::Value::default);
    let mut decision_note = use_signal(String::new);
    let mut initialized_for = use_signal(|| Option::<Uuid>::None);
    let draft = settings.cloned();
    let workflows = (store.workflows).cloned();
    let accounts = (store.accounts).cloned();

    use_effect(move || {
        let Some(id) = (store.selected_content).cloned() else { return };
        let Some(detail) = (store.content_detail).cloned().data else { return };
        if detail.item.id != id || initialized_for.cloned() == Some(id) {
            return;
        }
        gen_inputs.write().clear();
        release_text.set(String::new());
        not_before.set(format::default_local(Utc::now()));
        not_after.set(format::default_local(Utc::now() + ChronoDuration::hours(24)));
        schedule_at.set(format::default_local(Utc::now() + ChronoDuration::hours(1)));
        account_ids.set(Vec::new());
        // Start from the latest revision's frozen settings so a re-freeze only
        // changes what the operator actually edited.
        let frozen = detail
            .releases
            .first()
            .and_then(|release| release.actions.first())
            .map(|action| action.platform_settings.clone())
            .unwrap_or_else(|| serde_json::json!({}));
        // The editor starts empty; selecting an account syncs its channel's
        // drafts from the frozen value.
        settings.set(platform::SettingsDraft::default());
        settings_base.set(frozen);
        let mut drafts = alt.cloned();
        for asset in &detail.assets {
            drafts.entry(asset.id).or_insert_with(|| asset.alt_text.clone());
        }
        alt.set(drafts);
        initialized_for.set(Some(id));
    });

    let client_generate = client.clone();
    let generate = move |workflow_id: Uuid, default_inputs: &str| {
        let Some(content_id) = (store.selected_content).cloned() else { return };
        if initialized_for() != Some(content_id) {
            return;
        }
        let variant_count: i32 = variants.cloned().parse().unwrap_or(0);
        if !(1..=8).contains(&variant_count) {
            store.notify(false, "Variants must be between 1 and 8.");
            return;
        }
        let drafts = gen_inputs.read();
        let raw = drafts.get(&workflow_id).map_or(default_inputs, String::as_str);
        let inputs: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                store.notify(false, format!("Inputs must be valid JSON: {error}"));
                return;
            }
        };
        let body = GenerateIn { workflow_id, inputs, variants: variant_count };
        let client_generate = client_generate.clone();
        spawn(async move {
            match api::timed(client_generate.generate(content_id, &body)).await {
                Ok(generation) => {
                    store.notify(true, format!("Generation {} dispatched.", format::short_hash(&generation.id.to_string())));
                    api::load_content_detail(store, client_generate.clone(), content_id).await;
                    api::refresh_base(store, client_generate).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_freeze = client.clone();
    let freeze = move |_| {
        let Some(content_id) = (store.selected_content).cloned() else { return };
        let accounts = account_ids.cloned();
        if accounts.is_empty() {
            store.notify(false, "Select at least one account before freezing a release.");
            return;
        }
        let text = release_text.cloned().trim().to_string();
        if text.is_empty() {
            store.notify(false, "Release text cannot be empty.");
            return;
        }
        let (Some(not_before_at), Some(not_after_at)) =
            (format::parse_local(&not_before.cloned()), format::parse_local(&not_after.cloned()))
        else {
            store.notify(false, "Timing needs both a not-before and a not-after value.");
            return;
        };
        if not_after_at < not_before_at {
            store.notify(false, "The timing window ends before it starts.");
            return;
        }
        let conflicts = settings.cloned().conflicts();
        if !conflicts.is_empty() {
            let message = conflicts.iter().map(platform::Conflict::message).collect::<Vec<_>>().join(" ");
            store.notify(false, format!("Cannot freeze: {message}"));
            return;
        }
        let body = FreezeReleaseIn {
            account_ids: accounts,
            text,
            platform_settings: settings.cloned().apply(&settings_base.cloned()),
            reply_to: None,
            timing: TimingScope { not_before: not_before_at, not_after: not_after_at },
        };
        let client_freeze = client_freeze.clone();
        spawn(async move {
            match api::timed(client_freeze.freeze_release(content_id, &body)).await {
                Ok(release) => {
                    let frozen =
                        release.actions.first().map(|action| action.platform_settings.clone()).unwrap_or_else(|| serde_json::json!({}));
                    let roster = (store.accounts).cloned().data.unwrap_or_default();
                    let mut next = platform::SettingsDraft::default();
                    next.sync(&frozen, &platform::profiles_for(&roster, &account_ids.cloned()));
                    settings.set(next);
                    settings_base.set(frozen);
                    store.notify(true, format!("Release revision {} frozen.", release.revision));
                    api::load_content_detail(store, client_freeze.clone(), content_id).await;
                    api::refresh_base(store, client_freeze).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_request_review = client.clone();
    let request_review = move |release_id: Uuid| {
        let client_request_review = client_request_review.clone();
        spawn(async move {
            match api::timed(client_request_review.request_review(release_id)).await {
                Ok(release) => {
                    store.notify(true, format!("Release {} sent for review.", release.revision));
                    if let Some(content_id) = (store.selected_content).cloned() {
                        api::load_content_detail(store, client_request_review.clone(), content_id).await;
                    }
                    api::refresh_base(store, client_request_review).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_decide = client.clone();
    let decide_release = move |(release_id, approve): (Uuid, bool)| {
        let note = decision_note.cloned();
        let client_decide = client_decide.clone();
        spawn(async move {
            // Fetch the revision again: the operator decides on what the hub
            // holds now, not on a possibly stale detail pane.
            let outcome = async {
                let revision = api::timed(client_decide.release(release_id)).await?;
                let approval_id = revision.paperclip_approval_id.clone().ok_or_else(|| {
                    crate::state::UiError::new("missing_approval", "release has no Paperclip approval; request review first")
                })?;
                // The operator's own board key, read locally and never sent
                // to the hub.
                let board = contracts::client::PaperclipBoard::from_env()
                    .map_err(|error| crate::state::UiError::new("paperclip_config", error.to_string()))?;
                board
                    .decide(&approval_id, approve, (!note.is_empty()).then_some(note.as_str()))
                    .await
                    .map_err(|error| crate::state::UiError::new("paperclip", error.to_string()))?;
                api::timed(client_decide.link_approval(release_id, &LinkApprovalIn { approval_id })).await
            }
            .await;
            match outcome {
                Ok(revision) => {
                    store.notify(true, format!("Release {} is {}.", revision.revision, revision.state));
                    if let Some(content_id) = (store.selected_content).cloned() {
                        api::load_content_detail(store, client_decide.clone(), content_id).await;
                    }
                    api::refresh_base(store, client_decide).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };
    let reject_release = decide_release.clone();

    let client_schedule = client.clone();
    let schedule = move |release_id: Uuid| {
        let Some(due_at) = format::parse_local(&schedule_at.cloned()) else {
            store.notify(false, "Schedule needs a due time.");
            return;
        };
        let client_schedule = client_schedule.clone();
        spawn(async move {
            match api::timed(client_schedule.schedule(release_id, &ScheduleIn { due_at })).await {
                Ok(intents) => {
                    store.notify(true, format!("{} intent(s) scheduled.", intents.len()));
                    if let Some(content_id) = (store.selected_content).cloned() {
                        api::load_content_detail(store, client_schedule.clone(), content_id).await;
                    }
                    api::refresh_base(store, client_schedule).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let client_send_chat = client.clone();
    let send_chat = move |text: String| {
        let Some(content_id) = (store.selected_content).cloned() else { return };
        let client_send_chat = client_send_chat.clone();
        spawn(async move {
            match api::timed(client_send_chat.send_chat("content", content_id, &ChatMessageIn { body: text })).await {
                Ok(messages) => store.set_chat_content(Loaded::ready(messages)),
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
            }
        });
    };

    let review_proposal = move |index: usize| {
        let Some(messages) = (store.chat_content).cloned().data else { return };
        let Some(message) = messages.get(index) else { return };
        let Some(proposal) = message.proposal_id else {
            store.notify(false, "That message has no attached proposal.");
            return;
        };
        let persona = (store.content_detail)
            .cloned()
            .data
            .as_ref()
            .filter(|d| Some(d.item.id) == (store.selected_content).cloned())
            .map(|d| d.item.persona_id);
        if let Some(persona) = persona {
            store.set_selected_persona(Some(persona));
        }
        store.set_focus_proposal(Some(proposal));
        store.set_workspace(Workspace::Projects);
    };

    rsx! {
        aside { class: "context", aria_label: "Content items",
            div { class: "context-head",
                h2 { "Content" }
                span { class: "form-help nowrap-unit", if content_count == 1 { "1 item" } else { "{content_count} items" } }
            }
            div { class: "context-list",
                match content.data.clone() {
                    Some(items) if items.is_empty() => rsx! { p { class: "ctx-empty", "No content yet — create a brief from News." } },
                    Some(items) => rsx! {
                        for item in items {
                            {
                                let (state_label, _) = labels::content_state(item.state);
                                rsx! {
                                    ContextItem {
                                        key: "{item.id}",
                                        title: item.title.clone(),
                                        meta: format!("{} · {}", state_label, format::rel_time(item.updated_at.timestamp_millis().max(0) as u64)),
                                        active: Some(item.id) == selected_id,
                                        on_select: move |_| store.set_selected_content(Some(item.id)),
                                    }
                                }
                            }
                        }
                    },
                    None if content.error.is_none() => rsx! { p { class: "ctx-empty", "Loading content…" } },
                    None => rsx! {},
                }
            }
        }
        main { class: "work",
            if let Some(error) = content.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            if let Some(error) = detail_error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match selected {
                None if content.error.is_none() => rsx! { Loading { text: "Loading content…".to_string() } },
                None => rsx! { EmptyState { title: "Select a content item.".to_string(), hint: Some("Briefs created in News appear here.".to_string()) } },
                Some(detail) => rsx! {
                    {
                        let item = &detail.item;
                        let (state_label, state_tone) = labels::content_state(item.state);
                        // Releases arrive newest first.
                        let latest_release = detail.releases.first().cloned();
                        let persona_name = (store.personas)
                            .cloned()
                            .data
                            .and_then(|ps| ps.into_iter().find(|p| p.id == item.persona_id).map(|p| p.name))
                            .unwrap_or_else(|| "Individual unavailable".to_string());
                        let reviewable = detail.releases.iter().find(|r| r.state == ReleaseState::Draft).map(|r| r.id);
                        let approved = detail.releases.iter().find(|r| r.state == ReleaseState::Approved).cloned();
                        let review_hint = if latest_release.as_ref().is_some_and(|r| r.state == ReleaseState::InReview) {
                            "The latest release is already in review."
                        } else if accounts.data.as_ref().is_some_and(Vec::is_empty) {
                            "A publishing account is required to save a release draft."
                        } else {
                            "In Release, enter post text, choose an account, then click Save release draft."
                        };
                        let request_count = detail.generations.len();
                        let selected_workflow = workflows.data.as_ref().and_then(|items| {
                            items.iter().find(|workflow| Some(workflow.id) == workflow_id()).or_else(|| items.first())
                        });
                        let generation_workflow_id = selected_workflow.map(|workflow| workflow.id);
                        let has_prompt = selected_workflow.is_some_and(|workflow| workflow.bindings.get("prompt").is_some());
                        let default_inputs = if has_prompt {
                            let visual = &item.brief.visual_direction;
                            let prompt = if visual.trim().is_empty() { &item.title } else { visual };
                            serde_json::json!({ "prompt": prompt }).to_string()
                        } else {
                            "{}".to_string()
                        };
                        let inputs = generation_workflow_id
                            .filter(|_| initialized_for() == Some(item.id))
                            .and_then(|id| gen_inputs.read().get(&id).cloned())
                            .unwrap_or_else(|| default_inputs.clone());
                        rsx! {
                            div { class: "work-head",
                                div {
                                    h1 { "{item.title}" }
                                    p { class: "hint", "{persona_name} · updated {format::fmt_dt(item.updated_at)}" }
                                }
                                div { class: "spacer" }
                                 if item.state != contracts::ContentState::Brief {
                                     Chip { label: state_label.to_string(), tone: Some(state_tone.to_string()) }
                                 }
                                div { class: "form-field", style: "justify-items: start;",
                                    div { class: "row",
                                        button {
                                            class: "btn secondary",
                                            style: reviewable.is_none().then_some("opacity: 1; color: var(--text-muted); border-color: var(--border);"),
                                            disabled: reviewable.is_none(),
                                            aria_describedby: reviewable.is_none().then_some("studio-review-help"),
                                            onclick: move |_| {
                                                if let Some(id) = reviewable {
                                                    request_review(id);
                                                }
                                            },
                                            "Request review"
                                        }
                                        if reviewable.is_none() {
                                            button {
                                                class: "btn secondary",
                                                aria_controls: "studio-release-section",
                                                aria_describedby: "studio-review-help",
                                                onclick: move |_| {
                                                    let _ = document::eval(
                                                        "const section = document.getElementById('studio-release-section');
                                                        if (section) {
                                                            section.focus({ preventScroll: true });
                                                            const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
                                                            section.scrollIntoView({ behavior: reduced ? 'instant' : 'smooth', block: 'start' });
                                                        }",
                                                    );
                                                },
                                                "Go to Release ↓"
                                            }
                                        }
                                    }
                                    if reviewable.is_none() {
                                        p { class: "form-help", id: "studio-review-help", "{review_hint}" }
                                    }
                                }
                            }
                            Panel { title: "Brief".to_string(), caption: None,
                                 dl { class: "glass-card form-grid-2", style: "margin: 0;",
                                    for (label, value) in [
                                        ("Angle", item.brief.angle.as_str()),
                                        ("Audience", item.brief.audience.as_str()),
                                        ("Copy direction", item.brief.copy_direction.as_str()),
                                        ("Visual direction", item.brief.visual_direction.as_str()),
                                    ] {
                                        div { class: "kv-item", key: "{label}",
                                            dt { class: "kv-label", "{label}" }
                                             dd { class: "kv-value", style: "white-space: pre-wrap;", if value.trim().is_empty() { "—" } else { "{value}" } }
                                        }
                                    }
                                }
                            }
                             section { class: "panel",
                                 div { class: "panel-head",
                                     h3 { "Generation" }
                                     span { class: "form-help nowrap-unit", if request_count == 1 { "1 request" } else { "{request_count} requests" } }
                                 }
                                if let Some(error) = workflows.error.clone() {
                                    ErrorCard { code: error.code, message: error.message }
                                } else if workflows.loading {
                                    Loading { text: "Loading workflows…".to_string() }
                                }
                                div { class: "form-stack",
                                    div { class: "form-row",
                                        div { class: "form-field",
                                            label { class: "label-tech", r#for: "studio-workflow", "Workflow" }
                                            select {
                                                id: "studio-workflow", class: "input",
                                                value: generation_workflow_id.map(|id| id.to_string()).unwrap_or_default(),
                                                onchange: move |e| workflow_id.set(e.value().parse().ok()),
                                                if generation_workflow_id.is_none() {
                                                    option { value: "", "No workflows available" }
                                                }
                                                if let Some(items) = workflows.data.as_ref() {
                                                    for workflow in items {
                                                         option { key: "{workflow.id}", value: "{workflow.id}", "{workflow_label(&workflow.name)} · v{workflow.version}" }
                                                    }
                                                }
                                            }
                                        }
                                        div { class: "form-field", style: "flex: 0 1 8rem;",
                                            label { class: "label-tech", r#for: "studio-variants", "Variants" }
                                            input {
                                                id: "studio-variants", class: "input", r#type: "number",
                                                min: "1", max: "8", value: "{variants}",
                                                oninput: move |e| variants.set(e.value()),
                                            }
                                        }
                                    }
                                    if let Some(id) = generation_workflow_id {
                                        GenerationInputs {
                                            key: "{item.id}-{id}", inputs, has_prompt,
                                            on_change: move |value| { gen_inputs.write().insert(id, value); },
                                        }
                                    }
                                    div { class: "row",
                                        button {
                                            class: "btn primary",
                                            disabled: generation_workflow_id.is_none() || initialized_for() != Some(item.id),
                                            onclick: move |_| {
                                                if let Some(id) = generation_workflow_id {
                                                    generate(id, &default_inputs);
                                                }
                                            },
                                            "Generate variants"
                                        }
                                    }
                                }
                                if detail.generations.is_empty() {
                                    EmptyState { title: "No generations yet.".to_string(), hint: Some("Pick a workflow and generate variants.".to_string()) }
                                } else {
                                    div { class: "stack",
                                        for generation in detail.generations.iter().rev() {
                                            {
                                                let (gen_label, gen_tone) = labels::generation_state(generation.state);
                                                let (done, total) = api::attempt_progress(&generation.attempts);
                                                let pct = (done * 100).checked_div(total).unwrap_or(0);
                                                let cancellable = matches!(
                                                    generation.state,
                                                    contracts::GenerationState::Requested
                                                        | contracts::GenerationState::Dispatched
                                                        | contracts::GenerationState::Running
                                                        | contracts::GenerationState::Cancelling
                                                );
                                                let generation_id = generation.id;
                                                rsx! {
                                                    div { class: "glass-card generation", key: "{generation.id}",
                                                        div { class: "spread",
                                                            span { class: "label-tech", "Started {format::fmt_dt(generation.created_at)}" }
                                                            Chip { label: gen_label.to_string(), tone: Some(gen_tone.to_string()) }
                                                        }
                                                        div { class: "row",
                                                            span { class: "label-tech", "{generation.variants} variants · {done}/{total} steps complete" }
                                                        }
                                                        div { class: "progress", div { class: "fill", style: "width: {pct}%;" } }
                                                        if !generation.attempts.is_empty() {
                                                            div { class: "attempts",
                                                                for attempt in generation.attempts.iter() {
                                                                    {
                                                                        let (attempt_label, attempt_tone) = labels::attempt_state(attempt.state);
                                                                        rsx! {
                                                                            div { class: "attempt", key: "{attempt.id}",
                                                                                span { "{labels::attempt_kind(attempt.kind)}" }
                                                                                Chip { label: attempt_label.to_string(), tone: Some(attempt_tone.to_string()) }
                                                                                if let Some(error) = attempt.progress.get("error").and_then(Value::as_str) {
                                                                                    span { "{format::preview(error, 120)}" }
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        if cancellable {
                                                            {
                                                                let client = client.clone();
                                                                rsx! {
                                                                    DeltaRow {
                                                                        seq: format::short_hash(&generation.id.to_string()),
                                                                        category: "generation".to_string(),
                                                                        description: "cancel queued and running attempts".to_string(),
                                                                        action: Some("Cancel".to_string()),
                                                                        on_confirm: move |_| {
                                                                            let client = client.clone();
                                                                            spawn(async move {
                                                                                match api::timed(client.cancel_generation(generation_id)).await {
                                                                                    Ok(CancelOut { state, actions }) => {
                                                                                        store.notify(true, format!("Cancel {state}: {}", actions.join("; ")));
                                                                                        if let Some(content_id) = (store.selected_content).cloned() {
                                                                                            api::load_content_detail(store, client.clone(), content_id).await;
                                                                                        }
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
                            Panel { title: "Assets".to_string(), caption: Some(format!("{} asset(s)", detail.assets.len())),
                                if detail.assets.is_empty() {
                                    EmptyState { title: "No assets yet.".to_string(), hint: Some("Images and videos appear here after generation.".to_string()) }
                                } else {
                                    div { class: "segmented", role: "group", aria_label: "Asset layout",
                                        button { class: "seg-btn", aria_pressed: !four_up(), onclick: move |_| four_up.set(false), "2-up" }
                                        button { class: "seg-btn", aria_pressed: four_up(), onclick: move |_| four_up.set(true), "4-up" }
                                    }
                                    div { class: if four_up.cloned() { "asset-grid four" } else { "asset-grid two" },
                                        for asset in detail.assets.iter() {
                                            {
                                                let (asset_label, asset_tone) = labels::asset_state(asset.state);
                                                let asset_id = asset.id;
                                                let image = (store.asset_images).cloned().get(&asset.id).cloned();
                                                let current_alt = alt.cloned().get(&asset.id).cloned().unwrap_or_else(|| asset.alt_text.clone());
                                                rsx! {
                                                    div { class: if asset.selected { "glass-card asset-tile selected" } else { "glass-card asset-tile" }, key: "{asset.id}",
                                                        div { class: "asset-frame",
                                                            if let Some(uri) = image.clone() {
                                                                if format::is_video(&asset.mime) {
                                                                    video {
                                                                        class: "asset-video",
                                                                        controls: true,
                                                                        src: "{uri}",
                                                                        "video preview unavailable ({asset.mime})"
                                                                    }
                                                                } else {
                                                                    img { src: "{uri}", alt: "{current_alt}" }
                                                                }
                                                            } else {
                                                                span { class: "asset-missing", "{asset_label} · {asset.mime} · {asset.bytes} B" }
                                                            }
                                                        }
                                                        div { class: "asset-meta",
                                                            Chip { label: asset_label.to_string(), tone: Some(asset_tone.to_string()) }
                                                            span { class: "chip muted", "sha {format::short_hash(&asset.sha256)}" }
                                                            if asset.selected {
                                                                span { class: "chip sage", "selected" }
                                                            }
                                                            if format::is_video(&asset.mime) {
                                                                if let Some(uri) = image {
                                                                    a {
                                                                        class: "btn btn-glass btn-xs",
                                                                        href: "{uri}",
                                                                        download: "{format::media_name(&asset.mime, &asset.sha256)}",
                                                                        "Download"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        div { class: "field-row",
                                                            label { class: "label-tech", r#for: "alt-{asset.id}", "Alt text" }
                                                            input {
                                                                id: "alt-{asset.id}", class: "input", value: "{current_alt}",
                                                                oninput: move |e| {
                                                                    let mut drafts = alt.cloned();
                                                                    drafts.insert(asset_id, e.value());
                                                                    alt.set(drafts);
                                                                },
                                                            }
                                                        }
                                                        {
                                                            let client = client.clone();
                                                            let alt_text = current_alt.clone();
                                                            rsx! {
                                                                button {
                                                                    class: "btn btn-glass btn-xs",
                                                                    onclick: move |_| {
                                                                        let client = client.clone();
                                                                        let body = SelectAssetIn { selected: true, alt_text: Some(alt_text.clone()), ..Default::default() };
                                                                        spawn(async move {
                                                                            match api::timed(client.select_asset(asset_id, &body)).await {
                                                                                Ok(asset) => {
                                                                                    store.notify(true, format!("Asset {} selected.", format::short_hash(&asset.id.to_string())));
                                                                                    if let Some(content_id) = (store.selected_content).cloned() {
                                                                                        api::load_content_detail(store, client.clone(), content_id).await;
                                                                                    }
                                                                                }
                                                                                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
                                                                            }
                                                                        });
                                                                    },
                                                                    if asset.selected { "Select again" } else { "Select" }
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
                            Panel { title: "Platform settings".to_string(), caption: None,
                                div { class: "form-grid",
                                    if draft.drafts().is_empty() {
                                        if account_ids.cloned().is_empty() {
                                            p { class: "form-help", "Choose a publishing account below to see its settings." }
                                        } else {
                                            p { class: "form-help", "The selected channels have no additional settings." }
                                        }
                                    }
                                    for channel in draft.drafts() {
                                        {
                                            let profile = channel.profile();
                                            rsx! {
                                                div { class: "glass-card", key: "{profile.id}",
                                                    span { class: "label-tech", "{profile.label}" }
                                                    for field in profile.fields {
                                                        {
                                                            let key = field.key;
                                                            let label = field.label;
                                                            let id = format!("settings-{}-{key}", profile.id.replace(':', "-"));
                                                            let value = channel.text(key).to_string();
                                                            let checked = channel.flag(key);
                                                            match field.kind {
                                                                platform::Kind::Text => rsx! {
                                                                    div { class: "field-row",
                                                                        label { class: "label-tech", r#for: "{id}", "{label}" }
                                                                        input {
                                                                            id: "{id}", class: "input", value: "{value}",
                                                                            oninput: move |e| {
                                                                                let mut next = settings.cloned();
                                                                                next.set_text(profile.id, key, e.value());
                                                                                settings.set(next);
                                                                            },
                                                                        }
                                                                    }
                                                                },
                                                                platform::Kind::LongText => rsx! {
                                                                    div { class: "field-row",
                                                                        label { class: "label-tech", r#for: "{id}", "{label}" }
                                                                        textarea {
                                                                            id: "{id}", class: "input", value: "{value}",
                                                                            oninput: move |e| {
                                                                                let mut next = settings.cloned();
                                                                                next.set_text(profile.id, key, e.value());
                                                                                settings.set(next);
                                                                            },
                                                                        }
                                                                    }
                                                                },
                                                                platform::Kind::Tags => rsx! {
                                                                    div { class: "field-row",
                                                                        label { class: "label-tech", r#for: "{id}", "{label}" }
                                                                        input {
                                                                            id: "{id}", class: "input", placeholder: "comma separated",
                                                                            value: "{value}",
                                                                            oninput: move |e| {
                                                                                let mut next = settings.cloned();
                                                                                next.set_text(profile.id, key, e.value());
                                                                                settings.set(next);
                                                                            },
                                                                        }
                                                                    }
                                                                },
                                                                platform::Kind::Choice(values) => rsx! {
                                                                    div { class: "field-row",
                                                                        label { class: "label-tech", r#for: "{id}", "{label}" }
                                                                        select {
                                                                            id: "{id}", class: "input", value: "{value}",
                                                                            onchange: move |e| {
                                                                                let mut next = settings.cloned();
                                                                                next.set_text(profile.id, key, e.value());
                                                                                settings.set(next);
                                                                            },
                                                                            option { value: "", "channel default" }
                                                                            for choice in values {
                                                                                option { value: "{choice}", "{choice}" }
                                                                            }
                                                                            if !value.is_empty() && !values.contains(&value.as_str()) {
                                                                                option { value: "{value}", "{value}" }
                                                                            }
                                                                        }
                                                                    }
                                                                },
                                                                platform::Kind::Flag { .. } => rsx! {
                                                                    div { class: "field-row",
                                                                        label { class: "row",
                                                                            input {
                                                                                r#type: "checkbox", checked,
                                                                                onchange: move |e| {
                                                                                    let mut next = settings.cloned();
                                                                                    next.set_flag(profile.id, key, e.checked());
                                                                                    settings.set(next);
                                                                                },
                                                                            }
                                                                            "{label}"
                                                                        }
                                                                    }
                                                                },
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if !draft.drafts().is_empty() {
                                        p { class: "form-help", "If a channel has no disclosure field, include the disclosure in the post text." }
                                    }
                                }
                            }
                            section { class: "panel", id: "studio-release-section", tabindex: "-1", aria_labelledby: "studio-release-title",
                                div { class: "panel-head",
                                    h3 { id: "studio-release-title", "Release" }
                                    span { class: "label-tech", "{detail.releases.len()} revision(s)" }
                                }
                                if let Some(release) = latest_release.clone() {
                                    div { class: "glass-card",
                                        div { class: "spread",
                                            span { class: "label-tech", "revision {release.revision} · {release.actions.len()} action(s)" }
                                            {
                                                let (release_label, release_tone) = labels::release_state(release.state);
                                                rsx! { Chip { label: release_label.to_string(), tone: Some(release_tone.to_string()) } }
                                            }
                                        }
                                        if let Some(action) = release.actions.first() {
                                            p { class: "kv-value", "{action.text}" }
                                        }
                                        details {
                                            summary { class: "muted", "Release details" }
                                            div { class: "kv", span { class: "k", "Payload hash" }, span { class: "v", "{release.payload_hash}" } }
                                            if let Some(approval) = release.paperclip_approval_id.as_ref() {
                                                div { class: "kv", span { class: "k", "Paperclip approval" }, span { class: "v", "{approval}" } }
                                            }
                                            if let Some(action) = release.actions.first() {
                                                pre { class: "raw-json", "{serde_json::to_string_pretty(&action.platform_settings).unwrap_or_default()}" }
                                            }
                                        }
                                        if release.state == ReleaseState::InReview && release.paperclip_approval_id.is_some() {
                                            div { class: "form-grid",
                                                div { class: "field-row",
                                                    label { class: "label-tech", r#for: "decision-note", "Decision note (optional)" }
                                                    input {
                                                        id: "decision-note", class: "input", value: "{decision_note}",
                                                        oninput: move |e| decision_note.set(e.value()),
                                                    }
                                                }
                                                div { class: "form-actions",
                                                    {
                                                        let id = release.id;
                                                        rsx! { button { class: "btn btn-primary", onclick: move |_| decide_release((id, true)), "Approve" } }
                                                    }
                                                    {
                                                        let id = release.id;
                                                        rsx! { button { class: "btn btn-glass", onclick: move |_| reject_release((id, false)), "Reject" } }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                div { class: "form-grid",
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "release-text", "Text" }
                                        textarea {
                                            id: "release-text", class: "input",
                                            placeholder: "the exact post text",
                                            value: "{release_text}",
                                            oninput: move |e| release_text.set(e.value()),
                                        }
                                    }
                                    div { class: "field-row",
                                        label { class: "label-tech", "Accounts" }
                                        div { class: "stack",
                                            if let Some(error) = accounts.error.clone() {
                                                ErrorCard { code: error.code, message: error.message }
                                            } else if accounts.loading {
                                                Loading { text: "Loading accounts…".to_string() }
                                            }
                                            if let Some(items) = accounts.data.as_ref() {
                                                if items.is_empty() {
                                                    p { class: "label-tech", "No accounts registered." }
                                                }
                                                for account in items {
                                                    {
                                                        let id = account.id.clone();
                                                        let checked = account_ids.cloned().contains(&id);
                                                        rsx! {
                                                            label { class: "row", key: "{account.id}",
                                                                input {
                                                                    r#type: "checkbox",
                                                                    checked,
                                                                    onchange: move |e| {
                                                                        let mut current = account_ids.cloned();
                                                                        if e.checked() {
                                                                            if !current.contains(&id) { current.push(id.clone()); }
                                                                        } else {
                                                                            current.retain(|value| value != &id);
                                                                        }
                                                                        account_ids.set(current.clone());
                                                                        // Each selected channel contributes its
                                                                        // own keys; unselected ones are not written.
                                                                        let roster = (store.accounts).cloned().data.unwrap_or_default();
                                                                        let mut next = settings.cloned();
                                                                        next.sync(&settings_base.cloned(), &platform::profiles_for(&roster, &current));
                                                                        settings.set(next);
                                                                    },
                                                                }
                                                                "{account.handle} · {account.platform}"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "release-before", "Not before (UTC)" }
                                        input {
                                            id: "release-before", class: "input", r#type: "datetime-local",
                                            value: "{not_before}", oninput: move |e| not_before.set(e.value()),
                                        }
                                    }
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "release-after", "Not after (UTC)" }
                                        input {
                                            id: "release-after", class: "input", r#type: "datetime-local",
                                            value: "{not_after}", oninput: move |e| not_after.set(e.value()),
                                        }
                                    }
                                    div { class: "form-actions",
                                        button { class: "btn primary", onclick: freeze, "Save release draft" }
                                    }
                                }
                                if let Some(release) = approved.clone() {
                                    div { class: "glass-card",
                                        div { class: "spread",
                                            span { class: "label-tech", "approved revision {release.revision}" }
                                            span { class: "chip sage", "approved" }
                                        }
                                        div { class: "form-grid",
                                            div { class: "field-row",
                                                label { class: "label-tech", r#for: "schedule-at", "Due at (UTC)" }
                                                input {
                                                    id: "schedule-at", class: "input", r#type: "datetime-local",
                                                    value: "{schedule_at}", oninput: move |e| schedule_at.set(e.value()),
                                                }
                                            }
                                            div { class: "form-actions",
                                                {
                                                    let release_id = release.id;
                                                    rsx! { button { class: "btn btn-primary", onclick: move |_| schedule(release_id), "Schedule" } }
                                                }
                                            }
                                        }
                                    }
                                }
                                if detail.intents.is_empty() {
                                    EmptyState { title: "Nothing scheduled yet.".to_string(), hint: Some("Schedule an approved release to publish it.".to_string()) }
                                } else {
                                    div { class: "stack",
                                        for intent in detail.intents.iter() {
                                            {
                                                let (intent_label, intent_tone) = labels::intent_state(intent.state);
                                                let intent_id = intent.id;
                                                let account = accounts.data.as_ref()
                                                    .and_then(|items| items.iter().find(|account| account.id == intent.account_id))
                                                    .map(|account| account.handle.as_str())
                                                    .unwrap_or("Unknown account");
                                                let cancellable = matches!(
                                                    intent.state,
                                                    contracts::IntentState::Scheduled
                                                        | contracts::IntentState::Claimed
                                                        | contracts::IntentState::Submitting
                                                );
                                                rsx! {
                                                    div { class: "glass-card intent-row", key: "{intent.id}",
                                                        div { class: "spread",
                                                            span { class: "label-tech", "{account} · {format::fmt_dt(intent.due_at)}" }
                                                            Chip { label: intent_label.to_string(), tone: Some(intent_tone.to_string()) }
                                                        }
                                                        div { class: "row",
                                                            span { class: "label-tech", "schedule revision {intent.schedule_revision}" }
                                                            if let Some(url) = intent.provider_url.clone() {
                                                                button {
                                                                    class: "btn btn-glass btn-xs",
                                                                    onclick: move |_| api::open_external(&url),
                                                                    "Open post"
                                                                }
                                                            }
                                                        }
                                                        for attempt in intent.attempts.iter() {
                                                            div { class: "attempt", key: "{attempt.id}",
                                                                {
                                                                    let (attempt_label, attempt_tone) = labels::intent_state(attempt.state);
                                                                    rsx! {
                                                                        span { "Attempt {attempt.attempt}" }
                                                                        Chip { label: attempt_label.to_string(), tone: Some(attempt_tone.to_string()) }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        if cancellable {
                                                            {
                                                                let client = client.clone();
                                                                rsx! {
                                                                    DeltaRow {
                                                                        seq: format::short_hash(&intent.id.to_string()),
                                                                        category: "schedule".to_string(),
                                                                        description: "Cancel this scheduled post".to_string(),
                                                                        action: Some("Cancel".to_string()),
                                                                        on_confirm: move |_| {
                                                                            let client = client.clone();
                                                                            spawn(async move {
                                                                                match api::timed(client.cancel_intent(intent_id)).await {
                                                                                    Ok(intent) => {
                                                                                        store.notify(true, format!("Intent {} is {}.", format::short_hash(&intent.id.to_string()), intent.state));
                                                                                        if let Some(content_id) = (store.selected_content).cloned() {
                                                                                            api::load_content_detail(store, client.clone(), content_id).await;
                                                                                        }
                                                                                        api::refresh_base(store, client).await;
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
                            Panel { title: "Creative chat".to_string(), caption: Some("content-scoped".to_string()),
                                {
                                    let lines = transcript(&(store.chat_content).cloned().data.unwrap_or_default());
                                    rsx! {
                                        ChatPanel {
                                            lines,
                                            disabled: (store.selected_content).cloned().is_none(),
                                            placeholder: "ask for a different angle…".to_string(),
                                            on_send: send_chat,
                                            on_review: review_proposal,
                                        }
                                    }
                                }
                                if let Some(error) = (store.chat_content).cloned().error {
                                    ErrorCard { code: error.code, message: error.message }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn GenerationInputs(inputs: String, has_prompt: bool, on_change: EventHandler<String>) -> Element {
    let prompt = has_prompt.then(|| generation_prompt(&inputs)).flatten();
    let json_field = rsx! {
        div { class: "form-field",
            label { class: "label-tech", r#for: "studio-inputs", "Inputs (JSON)" }
            textarea {
                id: "studio-inputs", class: "input mono", value: "{inputs}", rows: "3",
                spellcheck: false,
                oninput: move |e| on_change.call(e.value()),
            }
        }
    };
    if !has_prompt {
        return json_field;
    }
    rsx! {
        if let Some(prompt) = prompt {
            div { class: "form-field",
                label { class: "label-tech", r#for: "studio-prompt", "Prompt" }
                textarea {
                    id: "studio-prompt", class: "input", value: "{prompt}", rows: "3",
                    oninput: move |e| {
                        if let Some(next) = set_generation_prompt(&inputs, e.value()) {
                            on_change.call(next);
                        }
                    },
                }
            }
        } else {
            p { class: "form-help", "To use the prompt field, add a text prompt in the JSON below." }
        }
        details {
            summary { class: "muted", "Advanced JSON inputs" }
            {json_field}
        }
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

#[cfg(test)]
mod tests {
    use super::{generation_prompt, set_generation_prompt};
    use serde_json::json;

    #[test]
    fn prompt_edits_preserve_advanced_inputs() {
        let raw = r#"{"prompt":"original","seed":42,"options":{"enabled":true}}"#;
        assert_eq!(generation_prompt(raw).as_deref(), Some("original"));
        let updated = set_generation_prompt(raw, "new\nprompt".into()).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&updated).unwrap(),
            json!({"prompt": "new\nprompt", "seed": 42, "options": {"enabled": true}})
        );
        for raw in ["{", "[]", r#"{"prompt":42}"#, "{}"] {
            assert_eq!(generation_prompt(raw), None);
            assert_eq!(set_generation_prompt(raw, "new".into()), None);
        }
    }
}
