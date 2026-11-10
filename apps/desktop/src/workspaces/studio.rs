//! Studio: content briefs, generation, asset selection, release freeze,
//! review, scheduling and delivery intents — all through `HubClient`.

use std::collections::HashMap;

use chrono::{Duration as ChronoDuration, Utc};
use contracts::client::HubClient;
use contracts::{CancelOut, ChatMessageIn, FreezeReleaseIn, GenerateIn, ReleaseState, ScheduleIn, SelectAssetIn, TimingScope};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{
        Chip, EmptyState, ErrorCard, Loading, Panel,
        chat::{ChatPanel, TranscriptLine},
        proposal::DeltaRow,
    },
    format, labels,
    shell::ContextItem,
    state::{Loaded, Store, Workspace},
};

#[component]
pub fn StudioView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let content = (store.content).cloned();
    let detail = (store.content_detail).cloned();
    let selected_id = (store.selected_content).cloned();
    let detail_error = detail.error.clone();
    let selected = detail.data.as_ref().filter(|d| Some(d.item.id) == selected_id).cloned();

    let mut workflow_id = use_signal(|| Option::<Uuid>::None);
    let mut variants = use_signal(|| "4".to_string());
    let mut gen_inputs = use_signal(String::new);
    let mut four_up = use_signal(|| false);
    let mut alt = use_signal(HashMap::<Uuid, String>::new);
    let mut release_text = use_signal(String::new);
    let mut account_ids = use_signal(Vec::<String>::new);
    let mut not_before = use_signal(String::new);
    let mut not_after = use_signal(String::new);
    let mut schedule_at = use_signal(String::new);
    let mut initialized_for = use_signal(|| Option::<Uuid>::None);

    use_effect(move || {
        if workflow_id.cloned().is_none() {
            workflow_id.set((store.workflows).cloned().data.and_then(|items| items.first().map(|w| w.id)));
        }
    });

    use_effect(move || {
        let Some(id) = (store.selected_content).cloned() else { return };
        let Some(detail) = (store.content_detail).cloned().data else { return };
        if detail.item.id != id || initialized_for.cloned() == Some(id) {
            return;
        }
        let brief = &detail.item.brief;
        gen_inputs.set(
            serde_json::json!({
                "prompt": brief.copy_direction,
                "angle": brief.angle,
                "audience": brief.audience,
                "visual": brief.visual_direction,
            })
            .to_string(),
        );
        release_text.set(String::new());
        not_before.set(format::default_local(Utc::now()));
        not_after.set(format::default_local(Utc::now() + ChronoDuration::hours(24)));
        schedule_at.set(format::default_local(Utc::now() + ChronoDuration::hours(1)));
        account_ids.set(Vec::new());
        let mut drafts = alt.cloned();
        for asset in &detail.assets {
            drafts.entry(asset.id).or_insert_with(|| asset.alt_text.clone());
        }
        alt.set(drafts);
        initialized_for.set(Some(id));
    });

    let client_generate = client.clone();
    let generate = move |_| {
        let Some(content_id) = (store.selected_content).cloned() else { return };
        let Some(workflow_id) = workflow_id.cloned() else {
            store.notify(false, "Select a workflow before generating.");
            return;
        };
        let variant_count: i32 = variants.cloned().parse().unwrap_or(0);
        if !(1..=8).contains(&variant_count) {
            store.notify(false, "Variants must be between 1 and 8.");
            return;
        }
        let inputs: serde_json::Value = match serde_json::from_str(&gen_inputs.cloned()) {
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
        let body = FreezeReleaseIn {
            account_ids: accounts,
            text,
            platform_settings: serde_json::json!({}),
            reply_to: None,
            timing: TimingScope { not_before: not_before_at, not_after: not_after_at },
        };
        let client_freeze = client_freeze.clone();
        spawn(async move {
            match api::timed(client_freeze.freeze_release(content_id, &body)).await {
                Ok(release) => {
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
        store.set_workspace(Workspace::Library);
    };

    rsx! {
        aside { class: "context", aria_label: "Content items",
            div { class: "context-head",
                h2 { "Content" }
                span { class: "label-tech", "{content.data.as_ref().map(|v| v.len()).unwrap_or(0)} items" }
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
                        let latest_release = detail.releases.last().cloned();
                        let reviewable = detail.releases.iter().rev().find(|r| r.state == ReleaseState::Draft).cloned();
                        let approved = detail.releases.iter().rev().find(|r| r.state == ReleaseState::Approved).cloned();
                        rsx! {
                            div { class: "work-head",
                                div {
                                    h1 { "{item.title}" }
                                    p { class: "hint", "persona {format::short_hash(&item.persona_id.to_string())} · updated {format::fmt_dt(item.updated_at)}" }
                                }
                                div { class: "spacer" }
                                Chip { label: state_label.to_string(), tone: Some(state_tone.to_string()) }
                                {
                                    let review_release = reviewable.clone();
                                    rsx! {
                                        button {
                                            class: "btn btn-glass",
                                            disabled: review_release.is_none(),
                                            onclick: move |_| {
                                                if let Some(release) = review_release.clone() {
                                                    request_review(release.id);
                                                }
                                            },
                                            "Request review"
                                        }
                                    }
                                }
                            }
                            Panel { title: "Brief".to_string(), caption: Some("frozen at creation".to_string()),
                                div { class: "kv-grid",
                                    div { class: "kv", span { class: "k", "Angle" }, span { class: "v", "{item.brief.angle}" } }
                                    div { class: "kv", span { class: "k", "Audience" }, span { class: "v", "{item.brief.audience}" } }
                                    div { class: "kv", span { class: "k", "Copy" }, span { class: "v", "{item.brief.copy_direction}" } }
                                    div { class: "kv", span { class: "k", "Visual" }, span { class: "v", "{item.brief.visual_direction}" } }
                                }
                            }
                            Panel { title: "Generation".to_string(), caption: Some(format!("{} request(s)", detail.generations.len())),
                                if let Some(error) = (store.workflows).cloned().error {
                                    ErrorCard { code: error.code, message: error.message }
                                } else if (store.workflows).cloned().data.is_none() {
                                    Loading { text: "Loading workflows…".to_string() }
                                }
                                div { class: "form-grid",
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "studio-workflow", "Workflow" }
                                        select {
                                            id: "studio-workflow",
                                            class: "input",
                                            value: "{workflow_id.cloned().map(|id| id.to_string()).unwrap_or_default()}",
                                            onchange: move |e| workflow_id.set(e.value().parse().ok()),
                                            if let Some(workflows) = (store.workflows).cloned().data {
                                                if workflows.is_empty() {
                                                    option { value: "", "No workflows registered" }
                                                }
                                                for workflow in workflows {
                                                    option { key: "{workflow.id}", value: "{workflow.id}", "{workflow.name} · v{workflow.version}" }
                                                }
                                            }
                                        }
                                    }
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "studio-variants", "Variants" }
                                        input {
                                            id: "studio-variants", class: "input", r#type: "number",
                                            min: "1", max: "8", value: "{variants}",
                                            oninput: move |e| variants.set(e.value()),
                                        }
                                    }
                                    div { class: "field-row",
                                        label { class: "label-tech", r#for: "studio-inputs", "Inputs (JSON)" }
                                        textarea {
                                            id: "studio-inputs", class: "input", value: "{gen_inputs}",
                                            oninput: move |e| gen_inputs.set(e.value()),
                                        }
                                    }
                                    div { class: "form-actions",
                                        button { class: "btn btn-primary", onclick: generate, "Generate variants" }
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
                                                            span { class: "label-tech", "generation {format::short_hash(&generation.id.to_string())}" }
                                                            Chip { label: gen_label.to_string(), tone: Some(gen_tone.to_string()) }
                                                        }
                                                        div { class: "row",
                                                            span { class: "label-tech", "{generation.variants} variants · request v{generation.request_version} · {done}/{total} attempts done" }
                                                        }
                                                        div { class: "progress", div { class: "fill", style: "width: {pct}%;" } }
                                                        if !generation.attempts.is_empty() {
                                                            div { class: "attempts",
                                                                for attempt in generation.attempts.iter() {
                                                                    {
                                                                        let (attempt_label, attempt_tone) = labels::attempt_state(attempt.state);
                                                                        rsx! {
                                                                            div { class: "attempt", key: "{attempt.id}",
                                                                                span { class: "external", "{labels::attempt_kind(attempt.kind)} · {attempt.external_id}" }
                                                                                Chip { label: attempt_label.to_string(), tone: Some(attempt_tone.to_string()) }
                                                                                span { "{format::preview(&serde_json::to_string(&attempt.progress).unwrap_or_default(), 60)}" }
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
                                div { class: "asset-toolbar",
                                    button {
                                        class: if four_up.cloned() { "btn btn-glass" } else { "btn btn-primary" },
                                        onclick: move |_| four_up.set(false),
                                        "2-up"
                                    }
                                    button {
                                        class: if four_up.cloned() { "btn btn-primary" } else { "btn btn-glass" },
                                        onclick: move |_| four_up.set(true),
                                        "4-up"
                                    }
                                }
                                if detail.assets.is_empty() {
                                    EmptyState { title: "No assets yet.".to_string(), hint: Some("Generated images appear here when the engine reports receipts.".to_string()) }
                                } else {
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
                                                            if let Some(uri) = image {
                                                                img { src: "{uri}", alt: "{current_alt}" }
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
                                                                        let body = SelectAssetIn { selected: true, alt_text: Some(alt_text.clone()) };
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
                            Panel { title: "Release".to_string(), caption: Some(format!("{} revision(s)", detail.releases.len())),
                                if let Some(release) = latest_release.clone() {
                                    div { class: "glass-card",
                                        div { class: "spread",
                                            span { class: "label-tech", "revision {release.revision} · {release.actions.len()} action(s)" }
                                            {
                                                let (release_label, release_tone) = labels::release_state(release.state);
                                                rsx! { Chip { label: release_label.to_string(), tone: Some(release_tone.to_string()) } }
                                            }
                                        }
                                        div { class: "kv", span { class: "k", "payload hash" }, span { class: "v", "{release.payload_hash}" } }
                                        if let Some(approval) = release.paperclip_approval_id.clone() {
                                            div { class: "kv", span { class: "k", "paperclip approval" }, span { class: "v", "{approval}" } }
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
                                            if let Some(error) = (store.accounts).cloned().error {
                                                ErrorCard { code: error.code, message: error.message }
                                            } else if (store.accounts).cloned().data.is_none() {
                                                Loading { text: "Loading accounts…".to_string() }
                                            }
                                            if let Some(accounts) = (store.accounts).cloned().data {
                                                if accounts.is_empty() {
                                                    p { class: "label-tech", "No accounts registered." }
                                                }
                                                for account in accounts {
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
                                                                        account_ids.set(current);
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
                                        button { class: "btn btn-primary", onclick: freeze, "Freeze release" }
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
                                    EmptyState { title: "No publication intents yet.".to_string(), hint: Some("Approved releases become intents when scheduled.".to_string()) }
                                } else {
                                    div { class: "stack",
                                        for intent in detail.intents.iter() {
                                            {
                                                let (intent_label, intent_tone) = labels::intent_state(intent.state);
                                                let intent_id = intent.id;
                                                let cancellable = matches!(
                                                    intent.state,
                                                    contracts::IntentState::Scheduled
                                                        | contracts::IntentState::Claimed
                                                        | contracts::IntentState::Submitting
                                                );
                                                rsx! {
                                                    div { class: "glass-card intent-row", key: "{intent.id}",
                                                        div { class: "spread",
                                                            span { class: "label-tech", "{intent.account_id} · due {format::fmt_dt(intent.due_at)}" }
                                                            Chip { label: intent_label.to_string(), tone: Some(intent_tone.to_string()) }
                                                        }
                                                        div { class: "row",
                                                            span { class: "label-tech", "schedule revision {intent.schedule_revision}" }
                                                            if let Some(post) = intent.provider_post_id.clone() {
                                                                span { class: "label-tech", "provider {post}" }
                                                            }
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
                                                                        span { class: "external", "attempt {attempt.attempt} · {attempt.idempotency_key}" }
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
                                                                        category: "intent".to_string(),
                                                                        description: "cancel this publication intent".to_string(),
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
