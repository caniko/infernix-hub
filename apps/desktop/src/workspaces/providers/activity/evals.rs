use super::*;

#[component]
pub(super) fn Evals(providers: Vec<ProviderSummary>, scoped_provider: Option<String>, seed: Option<EvalSeed>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut editing = use_signal(|| None::<Uuid>);
    let mut create = use_signal(|| seed.is_some());
    let mut revision = use_signal(|| 0u32);
    let data = use_resource(move || {
        let client = client.clone();
        let _ = (panel.epoch)();
        async move { timed(client.activity_evals()).await }
    });
    let result = data.read();
    rsx! {
        section { class: "glass-card stack",
            h2 { "Pinned eval cases" }
            p { class: "muted", "Tool-free text replay is explicit, can consume quota, and is not proof of model quality." }
            if panel.operator() {
                button { class: "btn btn-glass", onclick: move |_| { editing.set(None); create.set(true); revision += 1; }, "New case" }
            }
            {read_view(result.as_ref(), |cases| rsx! {
                if cases.is_empty() { p { class: "muted", "No eval cases saved." } }
                for case in cases.iter().filter(|case| scoped_provider.as_ref().is_none_or(|id| id == &case.provider_id)) {
                    {
                        let id = case.id;
                        rsx! { article { key: "{id}", class: "provider-inset stack",
                            div { class: "spread",
                                div { h3 { "{case.name}" } p { class: "muted", "{case.provider_id} · {case.model_id} · {text(&case.effort)}" } }
                                if panel.operator() {
                                    button { class: "btn btn-glass", onclick: move |_| { create.set(false); editing.set(Some(id)); revision += 1; }, "Edit case" }
                                }
                            }
                            EvalActions { case: case.clone(), provider: providers.iter().find(|p| p.descriptor.id == case.provider_id).cloned() }
                            if panel.operator() && editing() == Some(id) {
                                EvalEditor { key: "{id}-{revision}", providers: providers.clone(), initial: Some(case.clone()), seed: None, close: move |_| editing.set(None) }
                            }
                        } }
                    }
                }
            })}
            if panel.operator() && create() {
                EvalEditor { key: "new-{revision}", providers, initial: None, seed: if revision() == 0 { seed } else { None }, close: move |_| create.set(false) }
            }
        }
        if panel.operator() { EvalTransfer {} }
    }
}

#[component]
fn EvalEditor(providers: Vec<ProviderSummary>, initial: Option<EvalCase>, seed: Option<EvalSeed>, close: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let editing = initial.as_ref().map(|c| c.id);
    let mut draft = use_signal(|| match initial {
        Some(c) => EvalCaseIn {
            name: c.name,
            source_exchange_id: c.source_exchange_id,
            provider_id: c.provider_id,
            model_id: c.model_id,
            effort: c.effort,
            prompt_text: c.prompt_text,
            expected_outcome: c.expected_outcome,
            predicates: c.predicates,
        },
        None => EvalCaseIn {
            name: String::new(),
            source_exchange_id: seed.as_ref().map(|s| s.exchange.clone()),
            provider_id: seed
                .as_ref()
                .map(|s| s.provider.clone())
                .or_else(|| providers.first().map(|p| p.descriptor.id.clone()))
                .unwrap_or_default(),
            model_id: seed.as_ref().map(|s| s.model.clone()).unwrap_or_default(),
            effort: seed.and_then(|s| s.effort),
            prompt_text: String::new(),
            expected_outcome: "ok".into(),
            predicates: Vec::new(),
        },
    });
    let current = draft();
    let value = current.clone();
    let valid = [&current.name, &current.provider_id, &current.model_id, &current.prompt_text, &current.expected_outcome]
        .iter()
        .all(|s| !s.trim().is_empty())
        && current.prompt_text != "<omitted>"
        && current.predicates.iter().all(|p| !p.value.is_empty());
    let mut options = provider_choices(&providers);
    if !current.provider_id.is_empty() && !options.iter().any(|(id, _)| id == &current.provider_id) {
        options.push((current.provider_id.clone(), current.provider_id.clone()));
    }
    rsx! { article { class: "provider-inset stack",
        h3 { if editing.is_some() { "Edit eval case" } else { "Create eval case" } }
        if let Some(source) = &current.source_exchange_id {
            p { class: "muted", "Source exchange: {source}. Enter a reviewed text-only prompt; no captured body was loaded." }
        }
        div { class: "row",
            Field { label: "Case name", value: current.name, change: move |v| draft.write().name = v }
            Choice { label: "Provider", value: current.provider_id, options, change: move |v| draft.write().provider_id = v }
            Field { label: "Model ID", value: current.model_id, change: move |v| draft.write().model_id = v }
            Field { label: "Effort", value: current.effort.unwrap_or_default(), change: move |v: String| draft.write().effort = optional(&v) }
            Field { label: "Expected outcome", value: current.expected_outcome, change: move |v| draft.write().expected_outcome = v }
        }
        label { class: "provider-field", "Text-only prompt",
            textarea { class: "input", value: current.prompt_text, oninput: move |e| draft.write().prompt_text = e.value() }
        }
        for (i, predicate) in current.predicates.iter().enumerate() {
            div { class: "row",
                Choice { label: "Predicate", value: predicate_kind(predicate.kind), options: pairs(&[("contains", "Contains"), ("not", "NotContains"), ("equals", "Equals")]),
                    change: move |v: String| draft.write().predicates[i].kind = match v.as_str() { "not" => EvalPredicateKind::NotContains, "equals" => EvalPredicateKind::Equals, _ => EvalPredicateKind::Contains } }
                Field { label: "Expected text", value: predicate.value.clone(), change: move |v| draft.write().predicates[i].value = v }
                Check { label: "Case sensitive", value: predicate.case_sensitive, change: move |v| draft.write().predicates[i].case_sensitive = v }
                button { class: "btn btn-glass", onclick: move |_| { draft.write().predicates.remove(i); }, "Remove predicate" }
            }
        }
        div { class: "row",
            button { class: "btn btn-glass", onclick: move |_| draft.write().predicates.push(EvalPredicate {
                kind: EvalPredicateKind::Contains, value: String::new(), case_sensitive: false,
            }), "Add predicate" }
            button { class: "btn btn-primary", disabled: !valid || (panel.busy)(), onclick: move |_| {
                let value = value.clone();
                let client = client.clone();
                let Some(busy) = panel.begin() else { return; };
                spawn(async move {
                    let _busy = busy;
                    let result = match editing {
                        Some(id) => timed(client.update_eval_case(id, &value)).await,
                        None => timed(client.create_eval_case(&value)).await,
                    };
                    if result.is_ok() { panel.changed(); close.call(()); }
                    panel.report(result.map(|_| "Eval case saved.".into()));
                });
            }, "Save case" }
            button { class: "btn btn-glass", onclick: move |_| close.call(()), "Cancel" }
        }
    } }
}

fn predicate_kind(kind: EvalPredicateKind) -> &'static str {
    match kind {
        EvalPredicateKind::Contains => "contains",
        EvalPredicateKind::NotContains => "not",
        EvalPredicateKind::Equals => "equals",
    }
}

#[component]
fn EvalActions(case: EvalCase, provider: Option<ProviderSummary>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut replay = use_signal(|| None::<EvalReplayResult>);
    let mut delete = use_signal(|| false);
    let mut confirm_replay = use_signal(|| false);
    let id = case.id;
    let replay_client = client.clone();
    let can_replay = provider.as_ref().is_some_and(|p| supports(p, None, ProviderCapability::ReplayTextEval))
        && !case.prompt_text.trim().is_empty()
        && case.prompt_text != "<omitted>";
    let result = replay().or(case.last_replay.clone());
    rsx! {
        details { summary { "Expected outcome and predicates" }
            p { "Expected outcome: {case.expected_outcome}" }
            Table { label: "Expected predicates", headers: &["Predicate", "Expected text", "Case sensitive"],
                rows: case.predicates.iter().map(|p| vec![format!("{:?}", p.kind), p.value.clone(), p.case_sensitive.to_string()]).collect() }
        }
        if panel.operator() { div { class: "row",
            if can_replay {
                button { class: "btn btn-glass", disabled: (panel.busy)(), onclick: move |_| {
                    if !confirm_replay() { confirm_replay.set(true); return; }
                    confirm_replay.set(false);
                    let Some(busy) = panel.begin() else { return; };
                    let client = replay_client.clone();
                    spawn(async move {
                        let _busy = busy;
                        let result = timed_for(Duration::from_secs(90), client.replay_eval_case(id, &serde_json::json!({}))).await;
                        match result {
                            Ok(out) => { replay.set(Some(out)); panel.report(Ok("Replay result received.".into())); }
                            Err(e) => panel.report(Err(e)),
                        }
                    });
                }, if confirm_replay() { "Confirm tool-free Replay" } else { "Replay" } }
                if confirm_replay() {
                    button { class: "btn btn-glass", onclick: move |_| confirm_replay.set(false), "Cancel replay" }
                }
            }
            button { class: if delete() { "btn btn-danger-ghost" } else { "btn btn-glass danger" },
                disabled: (panel.busy)(), onclick: move |_| {
                    if !delete() { delete.set(true); return; }
                    delete.set(false);
                    let Some(busy) = panel.begin() else { return; };
                    let client = client.clone();
                    spawn(async move {
                        let _busy = busy;
                        let result = timed(client.delete_eval_case(id)).await;
                        if result.is_ok() { panel.changed(); }
                        panel.report(result.map(|_| "Eval case deleted.".into()));
                    });
                }, if delete() { "Confirm delete case" } else { "Delete case" }
            }
            if delete() { button { class: "btn btn-glass", onclick: move |_| delete.set(false), "Cancel delete" } }
        } }
        if let Some(result) = result {
            div { class: "provider-inset stack",
                h4 { "Replay · " if result.passed { "Passed" } else { "Failed" } }
                p { "{fmt_dt(result.replayed_at)} · {result.model_id} · expected {case.expected_outcome} / observed {result.actual_outcome}" }
                Table { label: "Predicate comparison", headers: &["Expected predicate", "Expected text", "Case sensitive", "Replay result"],
                    rows: result.predicate_results.iter().map(|p| vec![
                        format!("{:?}", p.predicate.kind), p.predicate.value.clone(), p.predicate.case_sensitive.to_string(),
                        if p.passed { "Pass" } else { "Fail" }.into()
                    ]).collect() }
                pre { class: "raw-json", "{result.response_preview}" }
            }
        }
    }
}

#[component]
fn EvalTransfer() -> Element {
    let client = use_context::<HubClient>();
    let import_client = client.clone();
    let panel = use_context::<PanelState>();
    let mut bodies = use_signal(|| false);
    let mut exported = use_signal(|| None::<String>);
    let mut input = use_signal(String::new);
    let mut preview = use_signal(|| None::<Result<EvalExportBundle, String>>);
    rsx! { section { class: "glass-card stack",
        h2 { "Eval JSON import / export" }
        p { class: "muted", "Export covers all saved cases, independent of Activity filters. Keep bodies off unless you intend to share prompt text and replay responses." }
        div { class: "row",
            Check { label: "Include case bodies", value: bodies(), disabled: (panel.busy)(), change: move |v| { exported.set(None); bodies.set(v); } }
            button { class: "btn btn-glass", disabled: (panel.busy)(), onclick: move |_| {
                let client = client.clone();
                let include = bodies();
                let Some(busy) = panel.begin() else { return; };
                exported.set(None);
                spawn(async move {
                    let _busy = busy;
                    let result = timed_for(Duration::from_secs(30), client.export_eval_cases(include)).await;
                    match result { Ok(bundle) => exported.set(Some(pretty(&bundle))), Err(e) => panel.report(Err(e)) }
                });
            }, "Export eval JSON" }
        }
        if let Some(value) = exported() { Output { label: "Eval JSON export", value, filename: "eval-cases.json" } }
        label { class: "provider-field", "Paste eval bundle JSON (maximum 2 MiB)",
            textarea { class: "input mono", value: input(), oninput: move |e| { input.set(e.value()); preview.set(None); } }
        }
        button { class: "btn btn-glass", disabled: input.read().trim().is_empty() || input.read().len() > 2 * 1024 * 1024, onclick: move |_| {
            preview.set(Some(serde_json::from_str::<EvalExportBundle>(&input()).map_err(|e| format!("Invalid eval bundle: {e}"))
                .and_then(|bundle| if bundle.schema_version == 1 && !bundle.cases.is_empty() { Ok(bundle) } else { Err("Unsupported schema version or empty bundle.".into()) })));
        }, "Preview import" }
        if let Some(Err(error)) = preview() { p { class: "monitoring-error", role: "alert", "{error}" } }
        if let Some(Ok(bundle)) = preview() {
            p { "Preview: {bundle.cases.len()} cases · schema {bundle.schema_version} · bodies included: {bundle.include_bodies}" }
            Table { label: "Eval import preview", headers: &["Case", "Provider", "Model", "Predicates"],
                rows: bundle.cases.iter().map(|c| vec![c.name.clone(), c.provider_id.clone(), c.model_id.clone(), c.predicates.len().to_string()]).collect() }
            button { class: "btn btn-primary", disabled: (panel.busy)(), onclick: move |_| {
                let bundle = bundle.clone();
                let client = import_client.clone();
                let Some(busy) = panel.begin() else { return; };
                spawn(async move {
                    let _busy = busy;
                    let result = timed_for(Duration::from_secs(40), client.import_eval_cases(&bundle)).await;
                    if result.is_ok() { panel.changed(); preview.set(None); input.set(String::new()); }
                    panel.report(result.map(|cases| format!("Imported {} eval cases.", cases.len())));
                });
            }, "Confirm import" }
        }
    } }
}
