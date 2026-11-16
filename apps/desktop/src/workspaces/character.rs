use contracts::{
    CanonBody, CanonDraftIn, CanonProposalIn, CanonSource, CharacterProfile, InfluencerMemberIn, Persona, ReadsAs, client::HubClient,
};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{EmptyState, ErrorCard, Panel, proposal::ListEditor},
    state::{Store, UiError},
};

pub fn bio_stage_done(profile: &CharacterProfile) -> bool {
    !profile.bio.is_empty() && profile.reads_as.is_some()
}

fn next_proposal(persona: &Persona, body: CanonBody) -> CanonProposalIn {
    CanonProposalIn { base_revision: persona.canon.as_ref().map(|r| r.revision), body, source: CanonSource::Operator, rationale: None }
}

#[component]
pub fn CharacterWizard(persona_id: Option<Uuid>, influencer_id: Option<Uuid>, on_close: EventHandler<()>) -> Element {
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
    let mut selected = use_signal(|| influencer_id);
    let mut role = use_signal(String::new);
    let mut step = use_signal(|| 0usize);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<UiError>);
    let mut has_draft = use_signal(|| initial.canon_draft.is_some());
    let mut member_added = use_signal(|| false);
    let mut proposal = use_signal(|| None::<Uuid>);
    let mut requested = use_signal(|| false);
    let mut saved_persona = use_signal(|| existing.clone());
    let influencers = (store.influencers).cloned().data.unwrap_or_default();
    let client_save = client.clone();
    let client_send = client.clone();
    let mut save = move |_: MouseEvent| {
        if busy() {
            return;
        }
        busy.set(true);
        error.set(None);
        let client = client_save.clone();
        let profile = CharacterProfile {
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
        let influencer = selected();
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
                && let Some(influencer) = influencer
            {
                if let Err(err) = api::timed(client.put_influencer_member(influencer, persona, &InfluencerMemberIn { role })).await {
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
        div { class: "influencer-dialog-backdrop",
            section { class: "influencer-dialog glass-card", role: "dialog", aria_modal: "true", aria_label: "Character wizard",
                div { class: "work-head", h2 { if persona_id.is_some() { "Edit character" } else { "New character" } }
                    button { class: "btn btn-glass", disabled: busy(), onclick: move |_| on_close.call(()), "Close" }
                }
                div { class: "row", for (index, label) in ["Bio", "Image", "Refine", "Review"].iter().enumerate() {
                    button { class: if step() == index { "btn btn-primary" } else { "btn btn-glass" }, disabled: busy() || (proposal().is_some() && index != 3), onclick: move |_| step.set(index), "{label}" }
                } }
                if let Some(err) = error() { ErrorCard { code: err.code, message: err.message } }
                if step() == 0 {
                    Panel { title: "Bio".to_string(), caption: None,
                        fieldset { class: "form-grid", disabled: busy(),
                            div { class: "field-row", label { r#for: "character-name", "Name" } input { id: "character-name", class: "input", disabled: persona_id.is_some() || id().is_some() || busy(), value: "{name}", oninput: move |e| name.set(e.value()) } }
                            if persona_id.is_none() {
                                div { class: "field-row", label { r#for: "character-influencer", "Influencer" } select { id: "character-influencer", class: "input", disabled: busy() || member_added(), value: selected().map(|v| v.to_string()).unwrap_or_default(), onchange: move |e| selected.set(Uuid::parse_str(&e.value()).ok()), option { value: "", "No influencer yet" } for item in influencers.clone() { option { value: "{item.id}", "{item.name}" } } } }
                                div { class: "field-row", label { r#for: "character-role", "Role" } input { id: "character-role", class: "input", disabled: busy() || member_added(), value: "{role}", oninput: move |e| role.set(e.value()) } }
                            }
                            div { class: "field-row", label { r#for: "character-reads", "Reads as" } select { id: "character-reads", class: "input", value: match reads() { Some(ReadsAs::Adult) => "adult", Some(ReadsAs::NonHuman) => "non_human", None => "" }, onchange: move |e| reads.set(match e.value().as_str() { "adult" => Some(ReadsAs::Adult), "non_human" => Some(ReadsAs::NonHuman), _ => None }), option { value: "", "Select…" } option { value: "adult", "Adult" } option { value: "non_human", "Non-human" } } }
                            div { class: "field-row", label { r#for: "character-bio", "Bio (kept verbatim)" } textarea { id: "character-bio", class: "input", value: "{bio}", oninput: move |e| bio.set(e.value()) } }
                            div { class: "field-row", label { r#for: "character-look", "Look description" } textarea { id: "character-look", class: "input", value: "{look}", oninput: move |e| look.set(e.value()) } }
                            ListEditor { title: "Palette".to_string(), hint: "Hex colours".to_string(), items: palette, placeholder: "#ffffff".to_string() }
                            div { class: "field-row", label { r#for: "character-voice", "Kokoro voice id" } input { id: "character-voice", class: "input", value: "{voice}", oninput: move |e| voice.set(e.value()) } }
                            button { class: "btn btn-glass", disabled: busy() || id().is_none(), onclick: move |_| {
                                let Some(persona) = id() else { return }; busy.set(true); error.set(None); let client = client.clone(); let input = CanonDraftIn { bio: bio() };
                                spawn(async move { match api::draft_canon(&client, persona, &input).await {
                                    Ok(body) => { canon_voice.set(body.voice); rules.set(body.rules); avoid.set(body.avoid); world.set(body.world); has_draft.set(true); },
                                    Err(err) => error.set(Some(err)),
                                } busy.set(false); });
                            }, "Draft canon from my bio" }
                            if id().is_none() { p { class: "hint", "Save the character before drafting canon." } }
                            if has_draft() {
                                div { class: "field-row", label { r#for: "character-canon-voice", "Canon voice" } textarea { id: "character-canon-voice", class: "input", value: "{canon_voice}", oninput: move |e| canon_voice.set(e.value()) } }
                                ListEditor { title: "Rules".to_string(), hint: "".to_string(), items: rules, placeholder: "Add a rule".to_string() }
                                ListEditor { title: "Avoid".to_string(), hint: "".to_string(), items: avoid, placeholder: "Add an item".to_string() }
                                ListEditor { title: "World".to_string(), hint: "".to_string(), items: world, placeholder: "Add a fact".to_string() }
                            }
                            button { class: "btn btn-primary", disabled: busy() || name().trim().is_empty(), onclick: save, "Save draft" }
                        }
                    }
                } else if step() == 3 {
                    { let body = CanonBody { voice: canon_voice(), rules: rules(), avoid: avoid(), world: world() };
                      let empty = body.voice.trim().is_empty() && body.rules.iter().chain(&body.avoid).chain(&body.world).all(|s| s.trim().is_empty());
                      let membership = influencers.iter().find_map(|i| i.members.iter().find(|m| Some(m.persona_id) == id()).map(|m| format!("{} · {}", i.name, m.role)))
                          .or_else(|| influencers.iter().find(|i| Some(i.id) == selected()).map(|i| format!("{} · {}", i.name, role()))).unwrap_or_else(|| "None".into());
                      let revision = existing.as_ref().and_then(|p| p.canon.as_ref()).map(|c| c.revision + 1).unwrap_or(1);
                      let reads_label = match reads() { Some(ReadsAs::Adult) => "Adult", Some(ReadsAs::NonHuman) => "Non-human", None => "—" };
                      rsx! {
                        Panel { title: "Review".to_string(), caption: Some("Read only".to_string()),
                            p { "Name: {name}" } p { "Influencer · role: {membership}" }
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
                                    let profile = CharacterProfile { bio: bio(), reads_as: reads(), look: look(), palette: palette(), voice: if voice().is_empty() { None } else { Some(voice()) }, canon_draft: Some(CanonBody { voice: canon_voice(), rules: rules(), avoid: avoid(), world: world() }) };
                                    let name = name(); let influencer = selected(); let role = role();
                                    spawn(async move {
                                        let persona = if let Some(persona_id) = id() {
                                            saved_persona().or_else(|| (store.personas).cloned().data.unwrap_or_default().into_iter().find(|p| p.id == persona_id))
                                        } else { match api::create_persona(&client, &name).await { Ok(p) => { id.set(Some(p.id)); saved_persona.set(Some(p.clone())); Some(p) }, Err(e) => { error.set(Some(e)); busy.set(false); return; } } };
                                        let Some(persona) = persona else { error.set(Some(UiError { code: "persona_not_loaded".into(), message: "Reload and try again.".into() })); busy.set(false); return };
                                        if proposal().is_none() && persona.pending_proposals > 0 {
                                            error.set(Some(UiError { code: "proposal_pending".into(), message: "A canon proposal is already pending. Send it for comments from the character page.".into() }));
                                            busy.set(false); return;
                                        }
                                        if proposal().is_none() {
                                            if let Err(e) = api::set_profile(&client, persona.id, &profile).await { error.set(Some(e)); busy.set(false); return; }
                                            if persona_id.is_none() && !member_added() && let Some(influencer) = influencer {
                                                if let Err(e) = api::timed(client.put_influencer_member(influencer, persona.id, &InfluencerMemberIn { role })).await { error.set(Some(e)); busy.set(false); return; }
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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_mode_proposes_against_the_accepted_revision() {
        let mut persona = Persona {
            id: Uuid::new_v4(),
            company_id: String::new(),
            name: String::new(),
            profile: CharacterProfile::default(),
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
        let mut profile = CharacterProfile::default();
        assert!(!bio_stage_done(&profile));
        profile.bio = "bio".into();
        assert!(!bio_stage_done(&profile));
        profile.reads_as = Some(ReadsAs::Adult);
        assert!(bio_stage_done(&profile));
        profile.bio.clear();
        assert!(!bio_stage_done(&profile));
    }
}
