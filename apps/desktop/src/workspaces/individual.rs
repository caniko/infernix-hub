use contracts::{
    Asset, AssetState, CanonBody, CanonDraftIn, CanonProposalIn, CanonSource, ImageCheck, IndividualLook, IndividualProfile, LookPassIn,
    LookSize, LookSourceIn, Persona, PinIn, ProjectMemberIn, ReadsAs, SelectAssetIn, client::HubClient,
};
use dioxus::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

use crate::{
    api,
    components::{Chip, EmptyState, ErrorCard, GatePrompt, Panel, proposal::ListEditor},
    format,
    state::{Store, UiError},
};

fn mime_for(file_name: &str) -> Option<&'static str> {
    match file_name.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

pub fn bio_stage_done(profile: &IndividualProfile) -> bool {
    !profile.bio.is_empty() && profile.reads_as.is_some()
}

fn next_proposal(persona: &Persona, body: CanonBody) -> CanonProposalIn {
    CanonProposalIn { base_revision: persona.canon.as_ref().map(|r| r.revision), body, source: CanonSource::Operator, rationale: None }
}

#[derive(Debug, PartialEq)]
struct PassNode {
    label: String,
    generation_id: Uuid,
    state: contracts::GenerationState,
    depth: usize,
    parent: Option<Uuid>,
}

fn pass_tree(look: &IndividualLook) -> Vec<PassNode> {
    let mut nodes: Vec<PassNode> = Vec::new();
    for (index, pass) in look.passes.iter().enumerate() {
        let parent = pass.generation.source_asset_id.and_then(|source| {
            look.passes[..index]
                .iter()
                .find(|candidate| candidate.assets.iter().any(|asset| asset.id == source))
                .map(|candidate| candidate.generation.id)
        });
        let depth = parent.and_then(|id| nodes.iter().find(|node| node.generation_id == id)).map_or(0, |node| node.depth + 1);
        nodes.push(PassNode {
            label: format!("P{}", index + 1),
            generation_id: pass.generation.id,
            state: pass.generation.state,
            depth,
            parent,
        });
    }
    nodes
}

fn lineage(look: &IndividualLook, asset_id: Uuid) -> Vec<String> {
    let mut labels = Vec::new();
    let mut current = Some(asset_id);
    let mut seen = BTreeSet::new();
    while let Some(id) = current.filter(|id| seen.insert(*id)) {
        if look.uploads.iter().any(|asset| asset.id == id) {
            labels.push("P0 upload".to_string());
            break;
        }
        let Some((index, pass)) = look.passes.iter().enumerate().find(|(_, pass)| pass.assets.iter().any(|asset| asset.id == id)) else {
            break;
        };
        labels.push(format!("P{}", index + 1));
        current = pass.generation.source_asset_id;
    }
    labels.reverse();
    labels
}

#[component]
fn PinButton(persona: Uuid, asset_id: Uuid, on_reload: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let store = use_context::<Store>();
    let mut busy = use_signal(|| false);
    let mut confirmation = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<UiError>);
    let pin = use_callback(move |confirm_repin| {
        if busy() {
            return;
        }
        busy.set(true);
        error.set(None);
        let client = client.clone();
        spawn(async move {
            match api::timed(client.pin_look_reference(persona, &PinIn { asset_id, confirm_repin })).await {
                Ok(out) => {
                    let mut details = Vec::new();
                    if !out.affected_projects.is_empty() {
                        details.push(format!("Affected projects: {}", out.affected_projects.join(", ")));
                    }
                    if !out.stale_stages.is_empty() {
                        details.push(format!("Stages to redo: {}", out.stale_stages.join(", ")));
                    }
                    store.notify(
                        true,
                        if details.is_empty() { "Reference pinned.".into() } else { format!("Reference pinned. {}", details.join(" · ")) },
                    );
                    on_reload.call(());
                    api::refresh_base(store, client).await;
                }
                Err(err) if err.code == "repin_needs_confirmation" && !confirm_repin => confirmation.set(Some(err.message)),
                Err(err) => error.set(Some(err)),
            }
            busy.set(false);
        });
    });
    rsx! {
        button { class: "btn btn-glass", disabled: busy(), onclick: move |_| pin.call(false), "Pin as reference" }
        if let Some(err) = error() { ErrorCard { code: err.code, message: err.message } }
        if let Some(message) = confirmation() {
            GatePrompt { label: format!("{message} 3D and rig built from the old reference need redoing."), approve_label: "Re-pin".to_string(), reject_label: "Cancel".to_string(),
                on_reject: move |_| confirmation.set(None),
                on_approve: move |_| { confirmation.set(None); pin.call(true); },
            }
        }
    }
}

fn final_needs_source(source: Option<Uuid>, size: LookSize) -> bool {
    source.is_none() && size == LookSize::Final
}

fn retry_input(generation: &contracts::GenerationRequest, workflow_name: &str) -> Option<LookPassIn> {
    Some(LookPassIn {
        source_asset_id: generation.source_asset_id,
        prompt: generation.inputs.get("prompt")?.as_str()?.to_string(),
        strength: generation.inputs.get("denoise").and_then(|v| v.as_f64()).map(|v| v as f32),
        variants: generation.variants,
        size: if workflow_name == "moss-look-img2img-final" { LookSize::Final } else { LookSize::Test },
        reuse_seeds_from: Some(generation.id),
    })
}

pub fn look_stage_done(reference: Option<&contracts::Reference>) -> bool {
    reference.is_some()
}

fn terminal(state: contracts::GenerationState) -> bool {
    matches!(
        state,
        contracts::GenerationState::Succeeded
            | contracts::GenerationState::Failed
            | contracts::GenerationState::Cancelled
            | contracts::GenerationState::OutcomeUnknown
    )
}

const REQUIRED_CHECKS: [(&str, &str); 3] = [
    ("adult_or_non_human", "Reads as an adult, or non-human"),
    ("not_sexualised", "Not sexualised"),
    ("not_real_person", "Not a real person and not modelled on one"),
];
const ADVICE_CHECKS: [(&str, &str); 2] =
    [("stylized", "Stylized rather than photoreal"), ("three_d_ready", "3D-ready: whole body, plain background, even light, A-pose")];

fn keep_request(_already: &[ImageCheck], ticked: &BTreeSet<String>, advice: &BTreeMap<String, bool>) -> SelectAssetIn {
    SelectAssetIn {
        selected: true,
        attest: REQUIRED_CHECKS.iter().filter(|(key, _)| ticked.contains(*key)).map(|(key, _)| (*key).into()).collect(),
        advice: advice.clone(),
        ..Default::default()
    }
}

#[component]
fn KeepDialog(asset: Asset, on_close: EventHandler<()>, on_reload: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let mut ticked = use_signal(|| {
        REQUIRED_CHECKS
            .iter()
            .filter(|(key, _)| asset.checks.iter().any(|check| check.check_key == *key && check.verdict == "pass"))
            .map(|(key, _)| (*key).to_string())
            .collect::<BTreeSet<_>>()
    });
    let mut advice = use_signal(|| {
        ADVICE_CHECKS
            .iter()
            .map(|(key, _)| {
                (
                    (*key).to_string(),
                    asset
                        .checks
                        .iter()
                        .filter(|check| check.check_key == *key)
                        .max_by_key(|check| check.checked_at)
                        .is_some_and(|check| check.verdict == "pass"),
                )
            })
            .collect::<BTreeMap<_, _>>()
    });
    let mut error = use_signal(|| None::<UiError>);
    let mut busy = use_signal(|| false);
    let valid = REQUIRED_CHECKS.iter().all(|(key, _)| ticked().contains(*key));
    rsx! {
        div { class: "project-dialog-backdrop",
            section { class: "project-dialog glass-card", role: "dialog", aria_modal: "true", aria_labelledby: "keep-image-title",
                h2 { id: "keep-image-title", "Keep image" }
                if let Some(err) = error() { ErrorCard { code: err.code, message: err.message } }
                fieldset { class: "form-grid", disabled: busy(), legend { "Required checks" }
                    for (key, label) in REQUIRED_CHECKS {
                        label { input { r#type: "checkbox", checked: ticked().contains(key), onchange: move |e| { if e.checked() { ticked.write().insert(key.to_string()); } else { ticked.write().remove(key); } } } " {label}" }
                    }
                }
                fieldset { class: "form-grid", disabled: busy(), legend { "Advice" }
                    for (key, label) in ADVICE_CHECKS {
                        label { input { r#type: "checkbox", checked: advice().get(key).copied().unwrap_or(false), onchange: move |e| { advice.write().insert(key.to_string(), e.checked()); } } " {label}" }
                    }
                }
                div { class: "form-actions",
                    button { class: "btn btn-glass", disabled: busy(), onclick: move |_| on_close.call(()), "Cancel" }
                    button { class: "btn btn-primary", disabled: busy() || !valid, onclick: move |_| {
                        if busy() || !valid { return; }
                        busy.set(true); error.set(None);
                        let input = keep_request(&asset.checks, &ticked(), &advice());
                        let client = client.clone();
                        spawn(async move {
                            match api::timed(client.select_asset(asset.id, &input)).await {
                                Ok(_) => { on_reload.call(()); on_close.call(()); },
                                Err(err) => error.set(Some(err)),
                            }
                            busy.set(false);
                        });
                    }, "Confirm" }
                }
            }
        }
    }
}

#[component]
fn LookAsset(
    asset: Asset,
    persona: Uuid,
    pinned: bool,
    on_source: EventHandler<Uuid>,
    on_keep: EventHandler<Asset>,
    on_reload: EventHandler<()>,
) -> Element {
    let store = use_context::<Store>();
    rsx! {
        div { class: "glass-card asset-tile",
            div { class: "asset-frame", if let Some(uri) = (store.asset_images).cloned().get(&asset.id) { img { src: "{uri}", alt: "{asset.alt_text}" } } else { span { class: "asset-missing", "{asset.mime}" } } }
            if asset.selected { Chip { label: "Kept".to_string(), tone: Some("ok".to_string()) } }
            if pinned { Chip { label: "Pinned".to_string(), tone: Some("ok".to_string()) } }
            for (index, check) in asset.checks.iter().enumerate().filter(|(index, check)| !asset.checks.iter().enumerate().any(|(other_index, other)| other.check_key == check.check_key && (other.checked_at, other_index) > (check.checked_at, *index))) {
                Chip { key: "{index}", label: format!("{} · {}", check.check_key, check.verdict), tone: Some(if check.verdict == "pass" { "ok" } else if ADVICE_CHECKS.iter().any(|(key, _)| *key == check.check_key) { "warn" } else { "err" }.to_string()) }
            }
            button { class: "btn btn-glass", onclick: move |_| on_source.call(asset.id), "Use as source" }
            button { class: "btn btn-glass", onclick: move |_| on_keep.call(asset.clone()), "Keep" }
            if asset.selected { PinButton { persona, asset_id: asset.id, on_reload } }
        }
    }
}

fn should_publish(ticket: u64, latest: u64) -> bool {
    ticket == latest
}

#[component]
pub fn IndividualWizard(persona_id: Option<Uuid>, project_id: Option<Uuid>, on_close: EventHandler<()>) -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let existing = (store.personas).cloned().data.unwrap_or_default().into_iter().find(|p| Some(p.id) == persona_id);
    let mut id = use_signal(|| persona_id);
    let mut name = use_signal(|| existing.as_ref().map(|p| p.name.clone()).unwrap_or_default());
    let initial = existing.as_ref().map(|p| p.profile.clone()).unwrap_or_default();
    let mut bio = use_signal(|| initial.bio.clone());
    let mut reads = use_signal(|| initial.reads_as.clone());
    let mut look = use_signal(|| initial.look.clone());
    let palette = use_signal(|| initial.palette.clone());
    let mut voice = use_signal(|| initial.voice.clone().unwrap_or_default());
    let draft = initial.canon_draft.clone().unwrap_or_default();
    let mut canon_voice = use_signal(|| draft.voice.clone());
    let mut rules = use_signal(|| draft.rules.clone());
    let mut avoid = use_signal(|| draft.avoid.clone());
    let mut world = use_signal(|| draft.world.clone());
    let mut selected = use_signal(|| project_id);
    let mut role = use_signal(String::new);
    let mut step = use_signal(|| 0usize);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<UiError>);
    let mut has_draft = use_signal(|| initial.canon_draft.is_some());
    let mut member_added = use_signal(|| false);
    let mut proposal = use_signal(|| None::<Uuid>);
    let mut requested = use_signal(|| false);
    let mut saved_persona = use_signal(|| existing.clone());
    let mut image_source = use_signal(|| 0usize);
    let mut image_file = use_signal(|| None::<dioxus::html::FileData>);
    let mut attested = use_signal(|| false);
    let mut image_prompt = use_signal(|| {
        let palette = initial.palette.join(", ");
        if palette.is_empty() { initial.look.clone() } else { format!("{}\nPalette: {palette}", initial.look) }
    });
    let mut prompt_initialized = use_signal(|| false);
    let mut variants = use_signal(|| 2i32);
    let mut individual_look = use_signal(|| None::<IndividualLook>);
    let mut library = use_signal(Vec::<Asset>::new);
    let mut refine_pass = use_signal(|| None::<Uuid>);
    let mut refine_source = use_signal(|| None::<Uuid>);
    let mut refine_size = use_signal(|| LookSize::Test);
    let mut refine_prompt = use_signal(|| image_prompt.cloned());
    let mut strength = use_signal(|| 0.35_f32);
    let mut refine_variants = use_signal(|| 2_i32);
    let mut reuse_seeds = use_signal(|| false);
    let mut workflows = use_signal(Vec::<contracts::Workflow>::new);
    let mut keep_asset = use_signal(|| None::<Asset>);
    let mut look_ticket = use_signal(|| 0_u64);
    let refresh_client = client.clone();
    let refresh_look = use_callback(move |persona: Uuid| {
        let ticket = *look_ticket.peek() + 1;
        look_ticket.set(ticket);
        let client = refresh_client.clone();
        spawn(async move {
            match api::timed(client.individual_look(persona)).await {
                Ok(look) if should_publish(ticket, *look_ticket.peek()) => individual_look.set(Some(look)),
                Err(err) if should_publish(ticket, *look_ticket.peek()) => error.set(Some(err)),
                _ => {}
            }
        });
    });
    let client_load = client.clone();
    use_effect(move || {
        if step() != 1 && step() != 2 && step() != 3 {
            return;
        }
        let Some(persona) = id() else { return };
        refresh_look.call(persona);
        let client = client_load.clone();
        spawn(async move {
            if step() != 1 {
                return;
            }
            match api::timed(client.assets()).await {
                Ok(assets) => {
                    let assets: Vec<_> =
                        assets.into_iter().filter(|a| a.state == AssetState::Ready && a.mime.starts_with("image/")).collect();
                    api::load_asset_bytes(store, client, &assets).await;
                    library.set(assets);
                }
                Err(err) => error.set(Some(err)),
            }
        });
    });
    let thumbnails_client = client.clone();
    use_effect(move || {
        let Some(look) = individual_look() else { return };
        let selected = refine_pass();
        let mut assets = look.uploads.clone();
        if let Some(pass) = look.passes.iter().find(|pass| Some(pass.generation.id) == selected) {
            assets.extend(pass.assets.clone());
        }
        if let Some(asset) = look.reference.as_ref().and_then(|reference| {
            look.uploads.iter().chain(look.passes.iter().flat_map(|pass| &pass.assets)).find(|asset| asset.id == reference.asset_id)
        }) {
            assets.push(asset.clone());
        }
        let client = thumbnails_client.clone();
        spawn(async move {
            for chunk in assets.chunks(8) {
                api::load_asset_bytes(store, client.clone(), chunk).await;
            }
        });
    });
    let workflow_client = client.clone();
    use_future(move || {
        let client = workflow_client.clone();
        async move {
            match api::timed(client.workflows()).await {
                Ok(items) => workflows.set(items),
                Err(err) => error.set(Some(err)),
            }
        }
    });
    let poll_client = client.clone();
    use_future(move || {
        let client = poll_client.clone();
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if *step.peek() != 2
                    || !individual_look.peek().as_ref().is_some_and(|look| look.passes.iter().any(|pass| !terminal(pass.generation.state)))
                {
                    continue;
                }
                let Some(persona) = *id.peek() else { continue };
                // Await the poll inline so polls never overlap and supersede each other;
                // explicit refreshes still take a newer ticket and win.
                let ticket = *look_ticket.peek() + 1;
                look_ticket.set(ticket);
                match api::timed(client.individual_look(persona)).await {
                    Ok(look) if should_publish(ticket, *look_ticket.peek()) => individual_look.set(Some(look)),
                    Err(err) if should_publish(ticket, *look_ticket.peek()) => error.set(Some(err)),
                    _ => {}
                }
            }
        }
    });
    let projects = (store.projects).cloned().data.unwrap_or_default();
    let client_save = client.clone();
    let client_send = client.clone();
    let mut save = move |_: MouseEvent| {
        if busy() {
            return;
        }
        busy.set(true);
        error.set(None);
        let client = client_save.clone();
        let profile = IndividualProfile {
            bio: bio(),
            reads_as: reads(),
            look: look(),
            palette: palette(),
            voice: if voice().is_empty() { None } else { Some(voice()) },
            canon_draft: if has_draft() {
                Some(CanonBody { voice: canon_voice(), rules: rules(), avoid: avoid(), world: world() })
            } else {
                None
            },
        };
        let name = name();
        let project = selected();
        let role = role();
        spawn(async move {
            let persona = if let Some(id) = id() {
                id
            } else {
                match api::create_persona(&client, &name).await {
                    Ok(persona) => {
                        id.set(Some(persona.id));
                        saved_persona.set(Some(persona.clone()));
                        persona.id
                    }
                    Err(err) => {
                        error.set(Some(err));
                        busy.set(false);
                        return;
                    }
                }
            };
            match api::set_profile(&client, persona, &profile).await {
                Ok(saved) => saved_persona.set(Some(saved)),
                Err(err) => {
                    error.set(Some(err));
                    busy.set(false);
                    return;
                }
            }
            if persona_id.is_none()
                && !member_added()
                && let Some(project) = project
            {
                if let Err(err) = api::timed(client.put_project_member(project, persona, &ProjectMemberIn { role })).await {
                    error.set(Some(err));
                    busy.set(false);
                    return;
                }
                member_added.set(true);
            }
            api::refresh_base(store, client).await;
            busy.set(false);
        });
    };
    rsx! {
        div { class: "project-dialog-backdrop",
            section { class: "project-dialog glass-card", role: "dialog", aria_modal: "true", aria_label: "Individual wizard",
                div { class: "work-head", h2 { if persona_id.is_some() { "Edit individual" } else { "New individual" } }
                    button { class: "btn btn-glass", disabled: busy(), onclick: move |_| on_close.call(()), "Close" }
                }
                div { class: "row", for (index, label) in ["Bio", "Image", "Refine", "Review"].iter().enumerate() {
                    button { class: if step() == index { "btn btn-primary" } else { "btn btn-glass" }, disabled: busy() || (proposal().is_some() && index != 3), onclick: move |_| { if index == 1 && !prompt_initialized() { let colours = palette().join(", "); image_prompt.set(if colours.is_empty() { look() } else { format!("{}\nPalette: {colours}", look()) }); prompt_initialized.set(true); } step.set(index); }, "{label}" }
                } }
                if let Some(err) = error() { ErrorCard { code: err.code, message: err.message } }
                if step() == 0 {
                    Panel { title: "Bio".to_string(), caption: None,
                        fieldset { class: "form-grid", disabled: busy(),
                            div { class: "field-row", label { r#for: "individual-name", "Name" } input { id: "individual-name", class: "input", disabled: persona_id.is_some() || id().is_some() || busy(), value: "{name}", oninput: move |e| name.set(e.value()) } }
                            if persona_id.is_none() {
                                div { class: "field-row", label { r#for: "individual-project", "Project" } select { id: "individual-project", class: "input", disabled: busy() || member_added(), value: selected().map(|v| v.to_string()).unwrap_or_default(), onchange: move |e| selected.set(Uuid::parse_str(&e.value()).ok()), option { value: "", "No project yet" } for item in projects.clone() { option { value: "{item.id}", "{item.name}" } } } }
                                div { class: "field-row", label { r#for: "individual-role", "Role" } input { id: "individual-role", class: "input", disabled: busy() || member_added(), value: "{role}", oninput: move |e| role.set(e.value()) } }
                            }
                            div { class: "field-row", label { r#for: "individual-reads", "Reads as" } select { id: "individual-reads", class: "input", value: match reads() { Some(ReadsAs::Adult) => "adult", Some(ReadsAs::NonHuman) => "non_human", None => "" }, onchange: move |e| reads.set(match e.value().as_str() { "adult" => Some(ReadsAs::Adult), "non_human" => Some(ReadsAs::NonHuman), _ => None }), option { value: "", "Select…" } option { value: "adult", "Adult" } option { value: "non_human", "Non-human" } } }
                            div { class: "field-row", label { r#for: "individual-bio", "Bio (kept verbatim)" } textarea { id: "individual-bio", class: "input", value: "{bio}", oninput: move |e| bio.set(e.value()) } }
                            div { class: "field-row", label { r#for: "individual-look", "Look description" } textarea { id: "individual-look", class: "input", value: "{look}", oninput: move |e| look.set(e.value()) } }
                            ListEditor { title: "Palette".to_string(), hint: "Hex colours".to_string(), items: palette, placeholder: "#ffffff".to_string() }
                            div { class: "form-field",
                                label { r#for: "individual-voice", "Voice preset" }
                                input { id: "individual-voice", class: "input", aria_describedby: "individual-voice-help", value: "{voice}", oninput: move |e| voice.set(e.value()) }
                                p { class: "form-help", id: "individual-voice-help", "Optional Kokoro preset, such as af_heart." }
                            }
                            button { class: "btn btn-glass", disabled: busy() || id().is_none(), onclick: move |_| {
                                let Some(persona) = id() else { return }; busy.set(true); error.set(None); let client = client.clone(); let input = CanonDraftIn { bio: bio() };
                                spawn(async move { match api::draft_canon(&client, persona, &input).await {
                                    Ok(body) => { canon_voice.set(body.voice); rules.set(body.rules); avoid.set(body.avoid); world.set(body.world); has_draft.set(true); },
                                    Err(err) => error.set(Some(err)),
                                } busy.set(false); });
                            }, "Draft canon from my bio" }
                            if id().is_none() { p { class: "hint", "Save the individual before drafting canon." } }
                            if has_draft() {
                                div { class: "field-row", label { r#for: "individual-canon-voice", "Canon voice" } textarea { id: "individual-canon-voice", class: "input", value: "{canon_voice}", oninput: move |e| canon_voice.set(e.value()) } }
                                ListEditor { title: "Rules".to_string(), hint: "".to_string(), items: rules, placeholder: "Add a rule".to_string() }
                                ListEditor { title: "Avoid".to_string(), hint: "".to_string(), items: avoid, placeholder: "Add an item".to_string() }
                                ListEditor { title: "World".to_string(), hint: "".to_string(), items: world, placeholder: "Add a fact".to_string() }
                            }
                            button { class: "btn btn-primary", disabled: busy() || name().trim().is_empty(), onclick: save, "Save draft" }
                        }
                    }
                } else if step() == 1 {
                    if let Some(persona) = id() {
                        div { class: "row", for (index, label) in ["Upload", "Generate from look", "Pick from Library"].iter().enumerate() {
                            button { class: if image_source() == index { "btn btn-primary" } else { "btn btn-glass" }, disabled: busy(), onclick: move |_| image_source.set(index), "{label}" }
                        } }
                        if image_source() == 0 {
                            Panel { title: "Upload".to_string(), caption: None,
                                label { r#for: "look-file", "Image (PNG, JPEG or WebP)" }
                                input { id: "look-file", r#type: "file", accept: ".png,.jpg,.jpeg,.webp", disabled: busy(), onchange: move |evt| image_file.set(evt.files().into_iter().next()) }
                                label { input { r#type: "checkbox", checked: attested(), disabled: busy(), onchange: move |evt| attested.set(evt.checked()) } "Not a photo of, or modelled on, a real person" }
                                button { class: "btn btn-primary", disabled: busy() || !attested() || image_file().is_none(), onclick: {
                                    let client = client.clone();
                                    move |_| {
                                        let Some(file) = image_file() else { return };
                                        let Some(mime) = mime_for(&file.name()) else { error.set(Some(UiError::new("invalid_image", "Choose a PNG, JPEG or WebP image."))); return };
                                        if file.size() > 32 * 1024 * 1024 { error.set(Some(UiError::new("invalid_image", "Image exceeds 32 MiB."))); return; }
                                        busy.set(true); error.set(None); let client = client.clone();
                                        spawn(async move {
                                            let result = match file.read_bytes().await {
                                                Ok(bytes) => api::timed(client.upload_look_image(persona, mime, true, bytes.to_vec())).await,
                                                Err(err) => Err(UiError::new("file_read", err.to_string())),
                                            };
                                            match result {
                                                Ok(_) => refresh_look.call(persona), Err(err) => error.set(Some(err)),
                                            } busy.set(false);
                                        });
                                    }
                                }, "Upload" }
                            }
                        } else if image_source() == 1 {
                            Panel { title: "Generate from look".to_string(), caption: None,
                                label { r#for: "look-prompt", "Prompt" }
                                textarea { id: "look-prompt", class: "input", disabled: busy(), value: "{image_prompt}", oninput: move |evt| image_prompt.set(evt.value()) }
                                label { r#for: "look-variants", "Variants" }
                                select { id: "look-variants", class: "input", disabled: busy(), value: "{variants}", onchange: move |evt| variants.set(evt.value().parse().unwrap_or(2)), for n in 1..=4 { option { value: "{n}", "{n}" } } }
                                button { class: "btn btn-primary", disabled: busy() || image_prompt().trim().is_empty(), onclick: {
                                    let client = client.clone(); move |_| {
                                        busy.set(true); error.set(None); let client = client.clone();
                                        let input = LookPassIn { source_asset_id: None, prompt: image_prompt(), strength: None, variants: variants(), size: LookSize::Test, reuse_seeds_from: None };
                                        spawn(async move {
                                            match api::timed(client.request_look_pass(persona, &input)).await {
                                                Ok(_) => refresh_look.call(persona),
                                                Err(err) => error.set(Some(err)),
                                            } busy.set(false);
                                        });
                                    }
                                }, "Generate" }
                            }
                        } else {
                            Panel { title: "Pick from Library".to_string(), caption: None,
                                div { class: "asset-grid four", for asset in library() {
                                    div { class: "glass-card asset-tile", key: "{asset.id}",
                                        div { class: "asset-frame", if let Some(uri) = (store.asset_images).cloned().get(&asset.id) { img { src: "{uri}", alt: "{asset.alt_text}" } } else { span { class: "asset-missing", "{asset.mime}" } } }
                                        span { class: "chip muted", "sha {format::short_hash(&asset.sha256)}" }
                                        button { class: "btn btn-glass", disabled: busy(), onclick: {
                                            let client = client.clone(); move |_| {
                                                busy.set(true); error.set(None); let client = client.clone(); let input = LookSourceIn { asset_id: asset.id };
                                                spawn(async move {
                                                    match api::timed(client.add_look_source(persona, &input)).await {
                                                        Ok(_) => refresh_look.call(persona),
                                                        Err(err) => error.set(Some(err)),
                                                    } busy.set(false);
                                                });
                                            }
                                        }, "Use image" }
                                    }
                                } }
                            }
                        }
                        if let Some(look) = individual_look() {
                            div { class: "asset-grid four", for asset in look.uploads {
                                LookAsset { key: "{asset.id}", pinned: look.reference.as_ref().is_some_and(|reference| reference.asset_id == asset.id), asset, persona, on_source: move |id| refine_source.set(Some(id)), on_keep: move |asset| keep_asset.set(Some(asset)), on_reload: move |_| refresh_look.call(persona) }
                            } }
                        }
                        Panel { title: "Checks".to_string(), caption: None,
                            p { "You confirm the required checks when you keep an image. No automated checker runs yet." }
                            ul { class: "facet-list", li { "Required · Reads as an adult, or is non-human" } li { "Required · Not sexualised" } li { "Required · Not a real person or modelled on one" } li { "Advice · Stylized rather than photoreal" } li { "Advice · Whole body in frame, plain background, even light, A-pose" } }
                        }
                    } else { p { "Save the individual first." } }
                } else if step() == 2 {
                    if let Some(look) = individual_look() {
                        div { style: "display: flex; flex-wrap: wrap; gap: 1rem; align-items: flex-start",
                        div { style: "flex: 2 1 18rem; min-width: 0",
                        Panel { title: "Passes".to_string(), caption: None,
                            if look_stage_done(look.reference.as_ref()) { Chip { label: "Look · done".to_string(), tone: Some("ok".to_string()) } }
                            for asset in look.uploads.clone() {
                                button { class: "btn btn-glass", onclick: move |_| refine_source.set(Some(asset.id)), "P0 · upload" }
                            }
                            for node in pass_tree(&look) {
                                { let index = look.passes.iter().position(|p| p.generation.id == node.generation_id).unwrap_or(0);
                                let generation = look.passes[index].generation.clone();
                                let previous = look.passes[..index].iter().position(|p| !terminal(p.generation.state));
                                let running = look.passes.iter().any(|p| matches!(p.generation.state, contracts::GenerationState::Running | contracts::GenerationState::Dispatched | contracts::GenerationState::Cancelling));
                                let workflow = workflows().iter().find(|w| w.id == generation.workflow_id).map(|w| w.name.clone());
                                let retry = workflow.as_deref().and_then(|name| retry_input(&generation, name));
                                 let indent = node.depth.min(3) * 16;
                                 rsx! { div { style: "padding-inline-start: min({indent}px, 20%);",
                                 button { class: "btn secondary", onclick: move |_| refine_pass.set(Some(node.generation_id)), "{node.label} · {node.state.as_str()}" }
                                if !terminal(generation.state) {
                                    if let Some(previous) = previous { p { "Queued behind P{previous + 1}" } }
                                    else if generation.state == contracts::GenerationState::Requested && !running { p { "Waiting for the GPU" } }
                                    button { class: "btn btn-glass", disabled: busy(), onclick: { let client = client.clone(); move |_| {
                                        let Some(persona) = id() else { return }; busy.set(true); error.set(None); let client = client.clone();
                                        spawn(async move { match api::timed(client.cancel_generation(generation.id)).await {
                                            Ok(_) => refresh_look.call(persona),
                                            Err(err) => error.set(Some(err)),
                                        } busy.set(false); });
                                    } }, "Cancel" }
                                } else if matches!(generation.state, contracts::GenerationState::Failed | contracts::GenerationState::Cancelled | contracts::GenerationState::OutcomeUnknown) {
                                    button { class: "btn btn-glass", disabled: busy() || retry.is_none(), onclick: { let client = client.clone(); move |_| {
                                        let (Some(persona), Some(input)) = (id(), retry.clone()) else { return }; busy.set(true); error.set(None); let client = client.clone();
                                        spawn(async move { match api::timed(client.request_look_pass(persona, &input)).await {
                                            Ok(_) => refresh_look.call(persona),
                                            Err(err) => error.set(Some(err)),
                                        } busy.set(false); });
                                    } }, "Retry" }
                                }
                                } } }
                            }
                        }
                        if let Some(pass) = look.passes.iter().find(|pass| Some(pass.generation.id) == refine_pass()) {
                            div { class: "asset-grid four", for asset in pass.assets.clone() {
                                if let Some(persona) = id() { LookAsset { key: "{asset.id}", pinned: look.reference.as_ref().is_some_and(|reference| reference.asset_id == asset.id), asset, persona, on_source: move |id| refine_source.set(Some(id)), on_keep: move |asset| keep_asset.set(Some(asset)), on_reload: move |_| refresh_look.call(persona) } }
                            } }
                        }
                        }
                        div { style: "flex: 1 1 14rem; min-width: 0",
                            Panel { title: "Next pass".to_string(), caption: None,
                                label { r#for: "refine-prompt", "Prompt (exactly what MOSS gets)" }
                                textarea { id: "refine-prompt", class: "input", disabled: busy(), value: "{refine_prompt}", oninput: move |e| refine_prompt.set(e.value()) }
                                label { r#for: "refine-strength", "Strength: {strength:.2}" }
                                input { id: "refine-strength", r#type: "range", min: "0.15", max: "0.85", step: "0.01", value: "{strength}", disabled: busy(), oninput: move |e| if let Ok(value) = e.value().parse() { strength.set(value) } }
                                div { class: "row", for preset in [0.15_f32, 0.35, 0.55, 0.85] { button { class: if strength() == preset { "btn btn-primary" } else { "btn btn-glass" }, disabled: busy(), onclick: move |_| strength.set(preset), "{preset:.2}" } } }
                                label { r#for: "refine-variants", "Variants" }
                                select { id: "refine-variants", class: "input", disabled: busy(), value: "{refine_variants}", onchange: move |e| refine_variants.set(e.value().parse().unwrap_or(2)), for n in 1..=4 { option { value: "{n}", "{n}" } } }
                                div { class: "row",
                                    button { class: if refine_size() == LookSize::Test { "btn btn-primary" } else { "btn btn-glass" }, disabled: busy(), onclick: move |_| refine_size.set(LookSize::Test), "Test" }
                                    button { class: if refine_size() == LookSize::Final { "btn btn-primary" } else { "btn btn-glass" }, disabled: busy() || final_needs_source(refine_source(), LookSize::Final), onclick: move |_| refine_size.set(LookSize::Final), "Final" }
                                }
                                label { input { r#type: "checkbox", checked: reuse_seeds(), disabled: busy() || refine_pass().is_none(), onchange: move |e| reuse_seeds.set(e.checked()) } " Reuse seeds of the selected pass" }
                                p { "MOSS · cfg 2.5 · shift 5 · er_sde/simple · 25 steps" }
                                if refine_size() == LookSize::Final { p { "About {refine_variants() as f64 * 179.0 / 60.0:.1} min" } } else { p { "about 3 min per image or less" } }
                                button { class: "btn btn-primary", disabled: busy() || refine_prompt().trim().is_empty() || final_needs_source(refine_source(), refine_size()), onclick: { let client = client.clone(); move |_| {
                                    let Some(persona) = id() else { return }; let source = refine_source();
                                    let input = LookPassIn { source_asset_id: source, prompt: refine_prompt(), strength: source.map(|_| strength()), variants: refine_variants(), size: refine_size(), reuse_seeds_from: if reuse_seeds() { refine_pass() } else { None } };
                                    busy.set(true); error.set(None); let client = client.clone();
                                    spawn(async move { match api::timed(client.request_look_pass(persona, &input)).await {
                                        Ok(_) => refresh_look.call(persona),
                                        Err(err) => error.set(Some(err)),
                                    } busy.set(false); });
                                } }, "Queue" }
                            }
                        }
                        }
                    } else { p { "Open Image to load this individual's look." } }
                } else if step() == 3 {
                    { let body = CanonBody { voice: canon_voice(), rules: rules(), avoid: avoid(), world: world() };
                      let empty = body.voice.trim().is_empty() && body.rules.iter().chain(&body.avoid).chain(&body.world).all(|s| s.trim().is_empty());
                      let membership = projects.iter().find_map(|i| i.members.iter().find(|m| Some(m.persona_id) == id()).map(|m| format!("{} · {}", i.name, m.role)))
                          .or_else(|| projects.iter().find(|i| Some(i.id) == selected()).map(|i| format!("{} · {}", i.name, role()))).unwrap_or_else(|| "None".into());
                      let revision = existing.as_ref().and_then(|p| p.canon.as_ref()).map(|c| c.revision + 1).unwrap_or(1);
                      let reads_label = match reads() { Some(ReadsAs::Adult) => "Adult", Some(ReadsAs::NonHuman) => "Non-human", None => "—" };
                      rsx! {
                        Panel { title: "Review".to_string(), caption: Some("Read only".to_string()),
                            h3 { "Pinned reference" }
                            if let Some(look) = individual_look() {
                                if let Some(reference) = look.reference.as_ref() {
                                    if let Some(asset) = look.uploads.iter().chain(look.passes.iter().flat_map(|pass| pass.assets.iter())).find(|asset| asset.id == reference.asset_id) {
                                        div { class: "asset-frame", if let Some(uri) = (store.asset_images).cloned().get(&asset.id) { img { src: "{uri}", alt: "{asset.alt_text}" } } else { span { class: "asset-missing", "{asset.mime}" } } }
                                    } else { p { "Reference image unavailable." } }
                                    p { "sha {format::short_hash(&reference.sha256)}" }
                                    p { "{lineage(&look, reference.asset_id).join(\" → \")}" }
                                } else { p { "No reference pinned yet." } }
                            } else { p { "No reference pinned yet." } }
                            p { "3D from the pinned image (TRELLIS.2) is offered on the individual page. It does not start by itself." }
                            p { "Name: {name}" } p { "Project · role: {membership}" }
                            p { "Reads as: {reads_label}" }
                            p { "Voice: {voice}" }
                            h3 { "Bio" } p { style: "white-space: pre-wrap", "{bio}" }
                            h3 { "Canon draft" } p { style: "white-space: pre-wrap", "{body.voice}" }
                            for (label, entries) in [("Rules", body.rules.clone()), ("Avoid", body.avoid.clone()), ("World", body.world.clone())] {
                                div { class: "facet", span { class: "label-tech", "{label}" } ul { class: "facet-list", for entry in entries { li { style: "white-space: pre-wrap", "{entry}" } } } }
                            }
                            h3 { "What happens next" }
                            ul { class: "facet-list", li { "Canon goes to the CG studio CEO agent for comments." } li { "You accept afterwards." } li { "Agents use only accepted canon." } li { "Nothing is posted." } }
                            div { class: "form-actions",
                                button { class: "btn btn-glass", disabled: busy() || name().trim().is_empty() || proposal().is_some(), onclick: move |e| { has_draft.set(true); save(e); }, "Save as draft (approves nothing)" }
                                button { class: "btn btn-primary", disabled: busy() || name().trim().is_empty() || (empty && proposal().is_none()), onclick: move |_| {
                                    if busy() { return; } busy.set(true); error.set(None);
                                    let client = client_send.clone();
                                    let profile = IndividualProfile { bio: bio(), reads_as: reads(), look: look(), palette: palette(), voice: if voice().is_empty() { None } else { Some(voice()) }, canon_draft: Some(CanonBody { voice: canon_voice(), rules: rules(), avoid: avoid(), world: world() }) };
                                    let name = name(); let project = selected(); let role = role();
                                    spawn(async move {
                                        let persona = if let Some(persona_id) = id() {
                                            saved_persona().or_else(|| (store.personas).cloned().data.unwrap_or_default().into_iter().find(|p| p.id == persona_id))
                                        } else { match api::create_persona(&client, &name).await { Ok(p) => { id.set(Some(p.id)); saved_persona.set(Some(p.clone())); Some(p) }, Err(e) => { error.set(Some(e)); busy.set(false); return; } } };
                                        let Some(persona) = persona else { error.set(Some(UiError::new("persona_not_loaded", "Reload and try again."))); busy.set(false); return };
                                        if proposal().is_none() && persona.pending_proposals > 0 {
                                            error.set(Some(UiError::new("proposal_pending", "A canon proposal is already pending. Send it for comments from the individual page.")));
                                            busy.set(false); return;
                                        }
                                        if proposal().is_none() {
                                            if let Err(e) = api::set_profile(&client, persona.id, &profile).await { error.set(Some(e)); busy.set(false); return; }
                                            if persona_id.is_none() && !member_added() && let Some(project) = project {
                                                if let Err(e) = api::timed(client.put_project_member(project, persona.id, &ProjectMemberIn { role })).await { error.set(Some(e)); busy.set(false); return; }
                                                member_added.set(true);
                                            }
                                            match api::timed(client.propose_canon(persona.id, &next_proposal(&persona, profile.canon_draft.clone().unwrap_or_default()))).await {
                                                Ok(rev) => proposal.set(Some(rev.id)), Err(e) => { error.set(Some(e)); busy.set(false); return; }
                                            }
                                        }
                                        if !requested() {
                                            if let Err(e) = api::timed(client.request_canon_comments(proposal().unwrap())).await { error.set(Some(e)); api::load_canon(store, client.clone(), persona.id).await; api::refresh_base(store, client).await; busy.set(false); return; }
                                            requested.set(true);
                                        }
                                        let mut profile = profile; profile.canon_draft = None;
                                        if let Err(e) = api::set_profile(&client, persona.id, &profile).await { error.set(Some(e)); busy.set(false); return; }
                                        store.set_selected_persona(Some(persona.id));
                                        api::refresh_base(store, client.clone()).await; api::load_canon(store, client, persona.id).await;
                                        busy.set(false); on_close.call(());
                                    });
                                }, if proposal().is_some() { "Retry sending for comments" } else if persona_id.is_some() { "Send canon r{revision} for comments" } else { "Create and send canon r1 for comments" } }
                            }
                        }
                      }
                    }
                }
                else { EmptyState { title: "Image steps arrive with the look slice".to_string(), hint: None } }
                if let (Some(asset), Some(persona)) = (keep_asset(), id()) {
                    KeepDialog { asset, on_close: move |_| keep_asset.set(None), on_reload: move |_| refresh_look.call(persona) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn newer_look_refresh_wins() {
        assert!(should_publish(2, 2));
        assert!(!should_publish(1, 2));
    }
    #[test]
    fn lineage_walks_back_to_the_upload() {
        let upload = asset();
        let first = asset();
        let last = asset();
        let look = IndividualLook {
            uploads: vec![upload.clone()],
            passes: vec![
                contracts::LookPass { generation: generation(Some(upload.id)), assets: vec![first.clone()] },
                contracts::LookPass { generation: generation(None), assets: vec![] },
                contracts::LookPass { generation: generation(Some(first.id)), assets: vec![last.clone()] },
            ],
            reference: None,
            reference_history: vec![],
        };
        assert_eq!(lineage(&look, last.id), ["P0 upload", "P1", "P3"]);
    }
    #[test]
    fn retry_reposts_the_same_inputs() {
        for (source, strength, size, workflow) in [
            (Some(Uuid::new_v4()), Some(0.35), LookSize::Final, "moss-look-img2img-final"),
            (None, None, LookSize::Test, "moss-look-txt2img"),
        ] {
            let mut generation = generation(source);
            generation.inputs = serde_json::json!({"prompt": "exact\ntext", "denoise": strength});
            generation.variants = 2;
            assert_eq!(
                retry_input(&generation, workflow),
                Some(LookPassIn {
                    source_asset_id: source,
                    prompt: "exact\ntext".into(),
                    strength,
                    variants: 2,
                    size,
                    reuse_seeds_from: Some(generation.id)
                })
            );
        }
    }
    #[test]
    fn look_stage_chip_is_done_only_with_a_pinned_reference() {
        assert!(!look_stage_done(None));
        let reference = contracts::Reference {
            id: Uuid::new_v4(),
            persona_id: Uuid::new_v4(),
            asset_id: Uuid::new_v4(),
            sha256: String::new(),
            pinned_by: String::new(),
            pinned_at: chrono::Utc::now(),
        };
        assert!(look_stage_done(Some(&reference)));
    }
    #[test]
    fn keep_sends_every_ticked_required_check() {
        let ticked = ["adult_or_non_human", "not_sexualised", "not_real_person"].into_iter().map(String::from).collect();
        let advice = [("stylized".to_string(), true), ("three_d_ready".to_string(), false)].into_iter().collect();
        let already = vec![contracts::ImageCheck {
            check_key: "not_real_person".into(),
            verdict: "pass".into(),
            checker: "operator".into(),
            detail: String::new(),
            checked_at: chrono::Utc::now(),
        }];
        let request = keep_request(&already, &ticked, &advice);
        assert!(request.selected);
        assert_eq!(request.attest.iter().cloned().collect::<std::collections::BTreeSet<_>>(), ticked);
        assert_eq!(request.advice, advice);
    }
    fn generation(source: Option<Uuid>) -> contracts::GenerationRequest {
        contracts::GenerationRequest {
            id: Uuid::new_v4(),
            content_item_id: None,
            persona_id: Some(Uuid::new_v4()),
            source_asset_id: source,
            workflow_id: Uuid::new_v4(),
            request_version: 1,
            inputs: serde_json::json!({"prompt":"test"}),
            variants: 1,
            state: contracts::GenerationState::Succeeded,
            attempts: vec![],
            created_at: chrono::Utc::now(),
        }
    }
    fn asset() -> Asset {
        Asset {
            id: Uuid::new_v4(),
            content_item_id: None,
            persona_id: Some(Uuid::new_v4()),
            generation_request_id: None,
            sha256: String::new(),
            mime: "image/png".into(),
            bytes: 0,
            state: AssetState::Ready,
            selected: false,
            alt_text: String::new(),
            provenance: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            checks: vec![],
        }
    }
    #[test]
    fn pass_tree_nests_a_pass_under_the_pass_that_made_its_source() {
        let upload = asset();
        let output = asset();
        let first = generation(Some(upload.id));
        let second = generation(Some(output.id));
        let third = generation(Some(upload.id));
        let look = IndividualLook {
            uploads: vec![upload],
            passes: vec![
                contracts::LookPass { generation: first.clone(), assets: vec![output] },
                contracts::LookPass { generation: second, assets: vec![] },
                contracts::LookPass { generation: third, assets: vec![] },
            ],
            reference: None,
            reference_history: vec![],
        };
        let nodes = pass_tree(&look);
        assert_eq!(
            nodes.iter().map(|n| (&*n.label, n.depth, n.parent)).collect::<Vec<_>>(),
            vec![("P1", 0, None), ("P2", 1, Some(first.id)), ("P3", 0, None)]
        );
    }
    #[test]
    fn a_pass_from_a_library_pick_is_a_root() {
        let look = IndividualLook {
            uploads: vec![asset()],
            passes: vec![contracts::LookPass { generation: generation(Some(Uuid::new_v4())), assets: vec![] }],
            reference: None,
            reference_history: vec![],
        };
        assert_eq!(pass_tree(&look)[0].parent, None);
    }
    #[test]
    fn final_size_needs_a_source() {
        assert!(final_needs_source(None, LookSize::Final));
        assert!(!final_needs_source(Some(Uuid::new_v4()), LookSize::Final));
        assert!(!final_needs_source(None, LookSize::Test));
    }
    #[test]
    fn upload_mime_comes_from_the_extension() {
        assert_eq!(mime_for("image.png"), Some("image/png"));
        assert_eq!(mime_for("image.JPG"), Some("image/jpeg"));
        assert_eq!(mime_for("image.jpeg"), Some("image/jpeg"));
        assert_eq!(mime_for("image.webp"), Some("image/webp"));
        assert_eq!(mime_for("image.txt"), None);
    }
    #[test]
    fn edit_mode_proposes_against_the_accepted_revision() {
        let mut persona = Persona {
            id: Uuid::new_v4(),
            company_id: String::new(),
            name: String::new(),
            profile: IndividualProfile::default(),
            canon: None,
            pending_proposals: 0,
            reference_asset_id: None,
        };
        assert_eq!(next_proposal(&persona, CanonBody::default()).base_revision, None);
        persona.canon = Some(contracts::CanonRevision {
            id: Uuid::new_v4(),
            persona_id: persona.id,
            revision: 2,
            base_revision: None,
            body: CanonBody::default(),
            state: contracts::CanonState::Accepted,
            source: CanonSource::Operator,
            rationale: None,
            proposed_by: String::new(),
            decided_by: None,
            created_at: chrono::Utc::now(),
            review_requested_at: None,
            review_issue_url: None,
            comments: vec![],
        });
        assert_eq!(next_proposal(&persona, CanonBody::default()).base_revision, Some(2));
    }
    #[test]
    fn bio_stage_chip_needs_a_bio_and_reads_as() {
        let mut profile = IndividualProfile::default();
        assert!(!bio_stage_done(&profile));
        profile.bio = "bio".into();
        assert!(!bio_stage_done(&profile));
        profile.reads_as = Some(ReadsAs::Adult);
        assert!(bio_stage_done(&profile));
        profile.bio.clear();
        assert!(!bio_stage_done(&profile));
    }
}
