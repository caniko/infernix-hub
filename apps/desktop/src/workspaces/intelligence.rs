//! Operator review of policy-scoped signals. Evidence and projections are text, never markup or navigation.

use chrono::{DateTime, TimeDelta, Utc};
use contracts::{
    Audience, IntelRunStatus, IntelSignal, IntelSignalDetail, IntelSignalQuery, IntelSignalReview, IntelSourceStatus, ProvenanceMethod,
    Severity, SignalClearanceIn, SignalDecisionAction, SignalDecisionIn, SignalState, client::HubClient,
};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{Chip, EmptyState, ErrorCard, Loading},
    format, labels,
    state::{Store, UiError},
};

#[component]
pub fn IntelligenceView() -> Element {
    let client = use_context::<HubClient>();
    let mut selected = use_context::<Store>().selected_signal;
    let mut query = use_signal(IntelSignalQuery::default);
    let source_client = client.clone();
    let mut sources = use_resource(move || {
        let client = source_client.clone();
        async move { api::intel_sources(&client).await }
    });
    let mut stream = use_resource(move || {
        let client = client.clone();
        let query = query.cloned();
        async move {
            let result = api::intel_signals(&client, &query).await;
            (query, result)
        }
    });
    let filters = query.cloned();
    let source_data = sources.cloned();
    let source_options = source_data.as_ref().and_then(|result| result.as_ref().ok());
    let page = stream.cloned().filter(|(loaded_query, _)| *loaded_query == filters);
    let has_signal_page = matches!(&page, Some((_, Ok(_))));
    let empty_title = match &page {
        Some((_, Ok(page))) if page.items.is_empty() => Some(empty_signal_title(&filters)),
        _ => None,
    };
    let empty_stream = is_empty_signal_stream(&filters, empty_title.is_some());
    let filtered = has_signal_filters(&filters);

    rsx! {
        aside { class: "context", aria_label: "Signal stream",
            div { class: "context-head", h2 { "Signals" } }
            div { class: "intel-filters", hidden: empty_stream, role: "group", aria_label: "Signal filters",
                label { class: "form-field", span { "Source" }
                    select { class: "input", value: filters.source.as_deref().unwrap_or_default(),
                        onchange: move |e| {
                            let mut q = query.write();
                            q.source = Some(e.value()).filter(|value| !value.is_empty());
                            q.cursor = None;
                        },
                        option { value: "", "All sources" }
                        if let Some(sources) = source_options {
                            for source in sources {
                                option { key: "{source.id}", value: "{source.id}", "{source.id}" }
                            }
                        }
                    }
                }
                label { class: "form-field", span { "State" }
                    select { class: "input", value: filters.state.map(SignalState::as_str).unwrap_or_default(),
                        onchange: move |e| {
                            let mut q = query.write();
                            q.state = SignalState::parse(&e.value());
                            q.cursor = None;
                        },
                        option { value: "", "All states" }
                        for state in SignalState::ALL {
                            option { value: state.as_str(), "{state_label(*state)}" }
                        }
                    }
                }
                label { class: "form-field", span { "Minimum severity" }
                    select { class: "input", value: filters.severity_min.map(Severity::as_str).unwrap_or_default(),
                        onchange: move |e| {
                            let mut q = query.write();
                            q.severity_min = Severity::parse(&e.value());
                            q.cursor = None;
                        },
                        option { value: "", "Any severity" }
                        for severity in Severity::ALL {
                            option { value: severity.as_str(), {labels::severity(*severity).0} }
                        }
                    }
                }
                label { class: "form-field", span { "Audience at most" }
                    select { class: "input", value: filters.audience_at_most.map(Audience::as_str).unwrap_or_default(),
                        onchange: move |e| {
                            let mut q = query.write();
                            q.audience_at_most = Audience::parse(&e.value());
                            q.cursor = None;
                        },
                        option { value: "", "Any audience" }
                        for audience in Audience::ALL {
                            option { value: audience.as_str(), "{audience_label(*audience)}" }
                        }
                    }
                }
            }
            div { class: "context-list",
                match page.map(|(_, result)| result) {
                    None => rsx! { Loading { text: "Loading signals…".to_string() } },
                    Some(Err(e)) => rsx! { ErrorCard { code: e.code, message: e.message } },
                    Some(Ok(page)) => rsx! {
                        if empty_title.is_some() {
                            p { class: "ctx-empty", if filtered { "0 matches" } else { "0 signals" } }
                            if filtered {
                                button {
                                    class: "btn secondary btn-xs",
                                    onclick: move |_| query.set(IntelSignalQuery::default()),
                                    "Reset filters"
                                }
                            }
                        }
                        for signal in page.items {
                            {
                                let id = signal.id;
                                rsx! {
                                    button {
                                        key: "{id}",
                                        class: if selected() == Some(id) { "context-item active" } else { "context-item" },
                                        aria_pressed: selected() == Some(id),
                                        onclick: move |_| selected.set(Some(id)),
                                        span { class: "ctx-title", style: "overflow-wrap: anywhere;", "{signal.title}" }
                                        {signal_badges(&signal)}
                                        span { class: "ctx-meta", "{state_label(signal.state)} · {age(signal.occurred_at)}" }
                                        span { class: "ctx-meta", style: "overflow-wrap: anywhere;", "Source: {signal.source_id}" }
                                    }
                                }
                            }
                        }
                        if let Some(cursor) = page.next_cursor {
                            button { class: "btn btn-glass btn-xs", onclick: move |_| query.write().cursor = Some(cursor.clone()), "Next page" }
                        }
                    },
                }
                if filters.cursor.is_some() {
                    button { class: "btn btn-glass btn-xs", onclick: move |_| query.write().cursor = None, "First page" }
                }
            }
        }
        main { class: "work",
            div { class: "content-bounded",
                div { class: "work-head",
                    h1 { "Intelligence" }
                    button { class: "btn btn-glass btn-xs", onclick: move |_| { sources.restart(); stream.restart(); }, "Refresh sources and signals" }
                }
                section { class: "panel",
                    h2 { "Source health" }
                    match source_data {
                        None => rsx! { Loading { text: "Loading sources…".to_string() } },
                        Some(Err(e)) => rsx! { ErrorCard { code: e.code, message: e.message } },
                        Some(Ok(items)) if items.is_empty() => rsx! { EmptyState { title: "No intelligence sources".to_string() } },
                        Some(Ok(items)) => rsx! {
                            div { class: "intel-source-grid",
                                for source in items {
                                    SourceHealth { key: "{source.company_id}/{source.id}", source }
                                }
                            }
                        },
                    }
                }
                if let Some(title) = empty_title {
                    EmptyState {
                        title: title.to_string(),
                        hint: empty_stream.then(|| "Set up a source if none are listed above, or check Source health for ingest errors. Then refresh sources and signals.".to_string()),
                    }
                } else if let Some(id) = selected() {
                    SignalDetail { key: "{id}", id, on_changed: move |_| stream.restart() }
                } else if has_signal_page {
                    EmptyState { title: "Select a signal".to_string() }
                }
            }
        }
    }
}

fn has_signal_filters(query: &IntelSignalQuery) -> bool {
    query.source.is_some() || query.state.is_some() || query.severity_min.is_some() || query.audience_at_most.is_some()
}

fn is_empty_signal_stream(query: &IntelSignalQuery, empty_page: bool) -> bool {
    empty_page && query.cursor.is_none() && !has_signal_filters(query)
}

fn empty_signal_title(query: &IntelSignalQuery) -> &'static str {
    if has_signal_filters(query) {
        "No matching signals."
    } else if query.cursor.is_some() {
        "No signals on this page."
    } else {
        "No signals yet."
    }
}

fn state_label(state: SignalState) -> &'static str {
    match state {
        SignalState::New => "New",
        SignalState::Acknowledged => "Acknowledged",
        SignalState::Promoted => "Promoted",
        SignalState::Dismissed => "Dismissed",
    }
}

fn audience_label(audience: Audience) -> &'static str {
    match audience {
        Audience::OperatorOnly => "Operator only",
        Audience::CompanyInternal => "Company internal",
        Audience::PersonaEligible => "Persona eligible",
        Audience::PublicCleared => "Cleared for public use",
    }
}

fn method_label(method: ProvenanceMethod) -> &'static str {
    match method {
        ProvenanceMethod::Observed => "Observed",
        ProvenanceMethod::Derived => "Derived",
        ProvenanceMethod::Llm => "LLM",
    }
}

fn age(at: DateTime<Utc>) -> String {
    format::rel_time(at.timestamp_millis().max(0) as u64)
}

#[component]
fn SourceHealth(source: IntelSourceStatus) -> Element {
    let (status, tone) = match (source.stale, source.last_status) {
        (true, _) => ("Stale", "warn"),
        (_, Some(IntelRunStatus::Ok)) => ("OK", "ok"),
        (_, Some(IntelRunStatus::Partial)) => ("Partial", "warn"),
        (_, Some(IntelRunStatus::Failed)) => ("Failed", "bad"),
        (_, None) => ("No runs", "muted"),
    };
    let last_ingest = source.last_ingest_at.map(age).unwrap_or_else(|| "never".to_string());
    rsx! {
        div { class: "glass-card intel-source-card",
            strong { "{source.id}" }
            Chip { label: status.to_string(), tone: Some(tone.to_string()) }
            span { class: "form-help", "Last ingest: {last_ingest}" }
            if let Some(code) = source.last_error_code {
                code { "{code}" }
            }
            if let Some(detail) = source.last_error_detail {
                p { class: "form-help", "{detail}" }
            }
        }
    }
}

fn signal_badges(signal: &IntelSignal) -> Element {
    let (severity, tone) = labels::severity(signal.severity);
    let confidence = signal.confidence * 100.0;
    rsx! {
        span { class: "row",
            Chip { label: severity.to_string(), tone: Some(tone.to_string()) }
            Chip { label: audience_label(signal.effective_audience).to_string() }
            Chip { label: format!("{} · {confidence:.0}%", method_label(signal.provenance.method)) }
        }
    }
}

#[component]
fn SignalDetail(id: Uuid, on_changed: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let mut generation = use_signal(|| 0u64);
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<UiError>);
    let mut notice = use_signal(|| None::<&'static str>);
    let detail_client = client.clone();
    let detail = use_resource(move || {
        let client = detail_client.clone();
        let version = generation();
        async move { (version, api::intel_signal(&client, id).await) }
    });
    let mutate = use_callback(move |request: Result<SignalDecisionIn, SignalClearanceIn>| {
        if *saving.peek() {
            return;
        }
        saving.set(true);
        error.set(None);
        notice.set(None);
        let client = client.clone();
        spawn(async move {
            let result = match request {
                Ok(input) => api::intel_decide(&client, id, &input).await,
                Err(input) => api::intel_clear(&client, id, &input).await,
            };
            match result {
                Ok(_) => {
                    generation += 1;
                    on_changed.call(());
                    notice.set(Some("Signal updated."));
                }
                Err(e) => {
                    if e.code == "stale_revision" {
                        generation += 1;
                    }
                    error.set(Some(e));
                }
            }
            saving.set(false);
        });
    });
    let version = generation();
    rsx! {
        if let Some(e) = error.cloned() {
            ErrorCard { code: e.code, message: e.message }
        }
        if let Some(message) = notice() {
            p { role: "status", "{message}" }
        }
        match detail.cloned().filter(|(loaded, _)| *loaded == version).map(|(_, result)| result) {
            None => rsx! { Loading { text: "Loading signal…".to_string() } },
            Some(Err(e)) => rsx! {
                ErrorCard { code: e.code, message: e.message }
                button { class: "btn btn-glass btn-xs", onclick: move |_| generation += 1, "Retry signal" }
            },
            Some(Ok(detail)) => {
                let review = detail.signal.review.clone();
                let source_default_audience = detail.signal.source_default_audience;
                rsx! {
                    SignalBody { detail }
                    section { class: "panel glass-card",
                        h2 { "Operator actions" }
                        div { class: "row",
                            for (label, action, hours) in [
                                ("Acknowledge", SignalDecisionAction::Acknowledge, None),
                                ("Dismiss", SignalDecisionAction::Dismiss, None),
                                ("Snooze 24h", SignalDecisionAction::Snooze, Some(24)),
                                ("Snooze 7d", SignalDecisionAction::Snooze, Some(168)),
                                ("Reopen", SignalDecisionAction::Reopen, None),
                            ] {
                                button {
                                    key: "{label}", class: "btn btn-glass btn-xs", disabled: saving(),
                                    onclick: move |_| mutate.call(Ok(SignalDecisionIn {
                                        action,
                                        until: hours.map(|hours| Utc::now() + TimeDelta::hours(hours)),
                                        note: None,
                                    })),
                                    "{label}"
                                }
                            }
                        }
                    }
                    if let Some(review) = review {
                        section { class: "panel glass-card",
                            h2 { "Audience clearance" }
                            ClearanceForm { key: "{version}", review, source_default_audience, saving: saving(), on_clear: move |input| mutate.call(Err(input)) }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SignalBody(detail: IntelSignalDetail) -> Element {
    let signal = &detail.signal;
    rsx! {
        section { class: "panel", style: "overflow-wrap: anywhere;",
            h2 { "{signal.title}" }
            {signal_badges(signal)}
            p { "{state_label(signal.state)} · {age(signal.occurred_at)}" }
            if let Some(until) = signal.snoozed_until {
                p { class: "form-help", "Snoozed until {format::fmt_dt(until)}" }
            }
            p { "{signal.summary}" }
            p { class: "form-help", "Source: {signal.source_id}" }
            h3 { "Evidence" }
            if signal.evidence.is_empty() { p { class: "form-help", "No evidence." } }
            ul {
                for evidence in &signal.evidence {
                    li {
                        code { "{evidence.url}" }
                        blockquote { "{evidence.quote}" }
                    }
                }
            }
            h3 { "Subjects" }
            if signal.subjects.is_empty() { p { class: "form-help", "No subjects." } }
            ul {
                for subject in &signal.subjects {
                    li { "{subject.scheme}: {subject.value}" }
                }
            }
            h3 { "Links" }
            if detail.links.is_empty() { p { class: "form-help", "No links." } }
            ul {
                for link in &detail.links {
                    li { "{link.relation}: {link.target_type} {link.target_id}" }
                }
            }
            details { class: "inspector-diagnostics",
                summary { "Diagnostics & raw payload" }
                div { class: "stack",
                    h3 { "Facts" }
                    pre { style: "white-space: pre-wrap;", "{signal.facts:#}" }
                    h3 { "Revision history" }
                    if detail.revisions.is_empty() { p { class: "form-help", "No visible revisions." } }
                    for revision in &detail.revisions {
                        details { key: "{revision.revision}",
                            summary { "Revision {revision.revision} · {format::fmt_dt(revision.received_at)}" }
                            code { "{revision.projection_sha256}" }
                            pre { style: "white-space: pre-wrap;", "{revision.payload:#}" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ClearanceForm(
    review: IntelSignalReview,
    source_default_audience: Audience,
    saving: bool,
    on_clear: EventHandler<SignalClearanceIn>,
) -> Element {
    let mut audience = use_signal(|| None::<Audience>);
    let mut rationale = use_signal(String::new);
    let ready = !saving && audience().is_some() && !rationale.read().trim().is_empty();
    rsx! {
        form { class: "stack",
            onsubmit: move |event| {
                event.prevent_default();
                let rationale = rationale.read().trim().to_string();
                if !saving && !rationale.is_empty() && let Some(audience) = audience() {
                    on_clear.call(SignalClearanceIn {
                        audience,
                        revision: review.revision,
                        policy_version: review.policy_version,
                        source_default_audience,
                        projection_sha256: review.projection_sha256.clone(),
                        rationale,
                    });
                }
            },
            dl { style: "overflow-wrap: anywhere;",
                dt { "Revision" } dd { "{review.revision}" }
                dt { "Policy version" } dd { "{review.policy_version}" }
                dt { "Projection SHA-256" } dd { code { "{review.projection_sha256}" } }
            }
            label { "Clearance audience"
                select { class: "input", required: true, disabled: saving, value: audience().map(Audience::as_str).unwrap_or_default(),
                    onchange: move |event| audience.set(Audience::parse(&event.value())),
                    option { value: "", "Choose audience" }
                    for level in Audience::ALL {
                        option { value: level.as_str(), "{audience_label(*level)}" }
                    }
                }
            }
            label { "Rationale"
                textarea { class: "input", required: true, disabled: saving, maxlength: 4000,
                    value: rationale(), oninput: move |event| rationale.set(event.value()),
                }
            }
            button { class: "btn btn-glass btn-xs", r#type: "submit", disabled: !ready, "Clear Audience" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_signal_pages_distinguish_filters_from_pagination() {
        let mut query = IntelSignalQuery::default();
        assert!(!has_signal_filters(&query));
        assert_eq!(empty_signal_title(&query), "No signals yet.");
        query.cursor = Some("next".into());
        assert!(!has_signal_filters(&query));
        assert_eq!(empty_signal_title(&query), "No signals on this page.");
        query.state = Some(SignalState::Acknowledged);
        assert!(has_signal_filters(&query));
        assert_eq!(empty_signal_title(&query), "No matching signals.");
    }

    #[test]
    fn only_an_empty_unfiltered_first_page_hides_signal_filters() {
        let mut query = IntelSignalQuery::default();
        assert!(is_empty_signal_stream(&query, true));
        assert!(!is_empty_signal_stream(&query, false));
        query.cursor = Some("next".into());
        assert!(!is_empty_signal_stream(&query, true));
        query.cursor = None;
        query.state = Some(SignalState::Acknowledged);
        assert!(!is_empty_signal_stream(&query, true));
    }

    #[test]
    fn signal_status_and_method_use_display_labels() {
        assert_eq!(state_label(SignalState::Acknowledged), "Acknowledged");
        assert_eq!(method_label(ProvenanceMethod::Observed), "Observed");
        assert_eq!(method_label(ProvenanceMethod::Llm), "LLM");
    }
}
