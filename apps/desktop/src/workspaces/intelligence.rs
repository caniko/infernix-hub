//! Review of policy-scoped signals. Evidence and projections are text, never markup or navigation.

use std::{collections::HashMap, time::Duration};

use chrono::{DateTime, TimeDelta, Utc};
use contracts::{
    Audience, ClearSignalIn, IntelRunStatus, IntelSignal, IntelSignalDetail, IntelSignalPage, IntelSignalQuery, IntelSignalReview,
    IntelSourceStatus, ProvenanceMethod, Role, Severity, SignalClearanceIn, SignalDecisionAction, SignalDecisionIn, SignalState,
    SystemStatus, client::HubClient,
};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{Chip, EmptyState, ErrorCard, Loading},
    format, labels,
    state::{Loaded, Store, UiError},
};

#[derive(Clone, Default)]
struct ClearanceDraft {
    audience: Option<Audience>,
    rationale: String,
    review_key: Option<ReviewKey>,
    reconfirm: bool,
}

type ReviewKey = (i32, i32, String, i64);

impl ClearanceDraft {
    fn invalidate_for(&mut self, review_key: &ReviewKey) {
        if self.audience.is_some() && self.review_key.as_ref() != Some(review_key) {
            self.audience = None;
            self.review_key = None;
            self.reconfirm = true;
        }
    }

    fn audience_for(&self, review_key: &ReviewKey) -> Option<Audience> {
        self.audience.filter(|_| self.review_key.as_ref() == Some(review_key))
    }
}

type ClearanceDrafts = Signal<HashMap<Uuid, ClearanceDraft>>;

fn use_revocation_poll<T: 'static>(status: Signal<Loaded<SystemStatus>>, mut resource: Resource<T>) {
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_millis(api::POLL_INTERVAL_MS)).await;
            if !resource.pending() && status.peek().data.as_ref().is_some_and(|status| status.principal.role != Role::Operator) {
                resource.restart();
            }
        }
    });
}

fn can_review(status: Option<&SystemStatus>) -> bool {
    status
        .is_some_and(|status| status.principal.role == Role::Operator || status.capabilities.as_ref().is_some_and(|caps| caps.intel_review))
}

fn clearance_audiences(status: Option<&SystemStatus>) -> &[Audience] {
    match status {
        Some(status) if status.principal.role == Role::Operator => Audience::ALL,
        Some(status) => {
            status.capabilities.as_ref().filter(|caps| caps.intel_clear).map_or(&[], |caps| caps.intel_clear_audiences.as_slice())
        }
        None => &[],
    }
}

fn page_revokes_selection(
    query: &IntelSignalQuery,
    result: &Result<IntelSignalPage, UiError>,
    id: Uuid,
    watched: Option<&(Uuid, IntelSignalQuery)>,
) -> bool {
    match result {
        Err(error) => error.access_lost(),
        // Changing filters or pages is not a revocation. A refresh of the
        // same page that held this selection is authoritative for that view.
        Ok(page) => {
            watched.is_some_and(|(watched_id, watched_query)| *watched_id == id && watched_query == query)
                && !page.items.iter().any(|item| item.id == id)
        }
    }
}

fn evict_signal(selected: &mut Option<Uuid>, drafts: &mut HashMap<Uuid, ClearanceDraft>, id: Uuid) {
    drafts.remove(&id);
    if *selected == Some(id) {
        *selected = None;
    }
}

fn discard_denied_clearance(drafts: &mut HashMap<Uuid, ClearanceDraft>, id: Uuid, error: &UiError) {
    if error.access_lost() {
        drafts.remove(&id);
    }
}

#[component]
pub fn IntelligenceView() -> Element {
    let client = use_context::<HubClient>();
    let store = use_context::<Store>();
    let mut selected = store.selected_signal;
    let mut query = use_signal(IntelSignalQuery::default);
    let mut drafts = use_signal(HashMap::new);
    let mut watched = use_signal(|| None::<(Uuid, IntelSignalQuery)>);
    let mut refresh = use_signal(|| 0u64);
    let source_client = client.clone();
    let mut sources = use_resource(move || {
        let client = source_client.clone();
        async move { api::intel_sources(&client).await }
    });
    let mut stream = use_resource(move || {
        let client = client.clone();
        let query = query.cloned();
        refresh();
        async move {
            let result = api::intel_signals(&client, &query).await;
            if result.as_ref().is_err_and(UiError::access_lost) {
                // This denial covers the stream, not just the selected row.
                drafts.write().clear();
            }
            let current = *selected.peek();
            if let Some(id) = current {
                if page_revokes_selection(&query, &result, id, watched.peek().as_ref()) {
                    evict_signal(&mut selected.write(), &mut drafts.write(), id);
                    watched.set(None);
                } else if result.as_ref().is_ok_and(|page| page.items.iter().any(|item| item.id == id)) {
                    watched.set(Some((id, query.clone())));
                }
            }
            (query, result)
        }
    });
    use_revocation_poll(store.status, stream);
    use_revocation_poll(store.status, sources);
    let filters = query.cloned();
    let source_data = sources.cloned();
    let source_options = source_data.as_ref().and_then(|result| result.as_ref().ok());
    let page = stream.cloned().filter(|(loaded_query, _)| *loaded_query == filters);
    let detail_revoked = move |id| {
        evict_signal(&mut selected.write(), &mut drafts.write(), id);
        stream.clear();
        stream.restart();
    };
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
                                        onclick: move |_| {
                                            watched.set(Some((id, query.peek().clone())));
                                            selected.set(Some(id));
                                        },
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
                    button { class: "btn btn-glass btn-xs", onclick: move |_| { sources.restart(); refresh += 1; }, "Refresh sources and signals" }
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
                }
                if let Some(id) = selected() {
                    SignalDetail {
                        key: "{id}", id, drafts, refresh, visible: empty_title.is_none(),
                        on_changed: move |_| stream.restart(), on_revoked: detail_revoked,
                    }
                } else if has_signal_page && empty_title.is_none() {
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
fn SignalDetail(
    id: Uuid,
    mut drafts: ClearanceDrafts,
    refresh: Signal<u64>,
    visible: bool,
    on_changed: EventHandler<()>,
    on_revoked: EventHandler<Uuid>,
) -> Element {
    let client = use_context::<HubClient>();
    let store = use_context::<Store>();
    let mut generation = use_signal(|| 0u64);
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<UiError>);
    let mut notice = use_signal(|| None::<&'static str>);
    let detail_client = client.clone();
    let detail = use_resource(move || {
        let client = detail_client.clone();
        let version = generation();
        refresh();
        async move {
            let result = api::intel_signal(&client, id).await;
            if result.as_ref().is_err_and(UiError::access_lost) {
                on_revoked.call(id);
            }
            (version, result)
        }
    });
    use_revocation_poll(store.status, detail);
    let mutate = use_callback(move |request: Result<SignalDecisionIn, ClearSignalIn>| {
        let status = store.status.peek();
        let permitted = match &request {
            Ok(_) => can_review(status.data.as_ref()),
            Err(input) => clearance_audiences(status.data.as_ref()).contains(&input.reviewed.audience),
        };
        if *saving.peek() || !permitted {
            return;
        }
        saving.set(true);
        error.set(None);
        notice.set(None);
        let client = client.clone();
        spawn(async move {
            let clearing = request.is_err();
            let result = match request {
                Ok(input) => api::intel_decide(&client, id, &input).await,
                Err(input) => api::intel_clear(&client, id, &input).await,
            };
            match result {
                Ok(_) => {
                    if clearing {
                        drafts.write().remove(&id);
                    }
                    generation += 1;
                    on_changed.call(());
                    notice.set(Some("Signal updated."));
                }
                Err(mut e) => {
                    if clearing {
                        discard_denied_clearance(&mut drafts.write(), id, &e);
                    }
                    if e.status == Some(409) && e.code == "stale_revision" {
                        e.message = "This signal changed while you were reviewing it. Reloading the latest revision; review it before trying again.".into();
                        generation += 1;
                    } else if e.access_lost() {
                        generation += 1;
                        if e.status == Some(404) {
                            on_revoked.call(id);
                        }
                    }
                    error.set(Some(e));
                }
            }
            saving.set(false);
        });
    });
    if !visible {
        return VNode::empty();
    }
    let version = generation();
    let status = store.status.read();
    let operator = status.data.as_ref().is_some_and(|status| status.principal.role == Role::Operator);
    let can_review = can_review(status.data.as_ref());
    let allowed_audiences = clearance_audiences(status.data.as_ref()).to_vec();
    drop(status);
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
                let clearance_generation = detail.signal.clearance_generation;
                rsx! {
                    SignalBody { detail }
                    if can_review && (operator || review.is_some()) {
                        section { class: "panel glass-card",
                            h2 { if operator { "Operator actions" } else { "Review actions" } }
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
                    }
                    if !allowed_audiences.is_empty() && let Some(review) = review {
                        section { class: "panel glass-card",
                            h2 { "Audience clearance" }
                            ClearanceForm {
                                id, drafts, review, source_default_audience, allowed_audiences, clearance_generation,
                                saving: saving(), on_clear: move |input| mutate.call(Err(input)),
                            }
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
    id: Uuid,
    mut drafts: ClearanceDrafts,
    review: IntelSignalReview,
    source_default_audience: Audience,
    allowed_audiences: Vec<Audience>,
    clearance_generation: i64,
    saving: bool,
    on_clear: EventHandler<ClearSignalIn>,
) -> Element {
    let review_key = (review.revision, review.policy_version, review.projection_sha256.clone(), clearance_generation);
    if drafts.peek().get(&id).is_some_and(|draft| draft.audience.is_some() && draft.review_key.as_ref() != Some(&review_key))
        && let Some(draft) = drafts.write().get_mut(&id)
    {
        draft.invalidate_for(&review_key);
    }
    let draft = drafts.read().get(&id).cloned().unwrap_or_default();
    let audience = draft.audience_for(&review_key).filter(|audience| allowed_audiences.contains(audience));
    let submit_audiences = allowed_audiences.clone();
    let change_audiences = allowed_audiences.clone();
    let reconfirm = draft.reconfirm;
    let choice_key = review_key.clone();
    let rationale = draft.rationale;
    let ready = !saving && audience.is_some() && !rationale.trim().is_empty();
    rsx! {
        form { class: "stack",
            onsubmit: move |event| {
                event.prevent_default();
                let drafts = drafts.peek();
                if !saving && let Some(draft) = drafts.get(&id)
                    && let Some(audience) = draft.audience_for(&review_key).filter(|audience| submit_audiences.contains(audience))
                    && !draft.rationale.trim().is_empty()
                {
                    on_clear.call(ClearSignalIn {
                        reviewed: SignalClearanceIn {
                            audience,
                            revision: review.revision,
                            policy_version: review.policy_version,
                            source_default_audience,
                            projection_sha256: review.projection_sha256.clone(),
                            rationale: draft.rationale.trim().to_string(),
                        },
                        expected_clearance_generation: Some(clearance_generation),
                    });
                }
            },
            dl { style: "overflow-wrap: anywhere;",
                dt { "Revision" } dd { "{review.revision}" }
                dt { "Policy version" } dd { "{review.policy_version}" }
                dt { "Projection SHA-256" } dd { code { "{review.projection_sha256}" } }
            }
            label { "Clearance audience"
                select { class: "input", required: true, disabled: saving, value: audience.map(Audience::as_str).unwrap_or_default(),
                    onchange: move |event| {
                        let mut drafts = drafts.write();
                        let draft = drafts.entry(id).or_default();
                        draft.audience = Audience::parse(&event.value()).filter(|audience| change_audiences.contains(audience));
                        draft.review_key = draft.audience.map(|_| choice_key.clone());
                        if draft.audience.is_some() {
                            draft.reconfirm = false;
                        }
                    },
                    option { value: "", selected: audience.is_none(), "Choose audience" }
                    for level in Audience::ALL.iter().copied().filter(|level| allowed_audiences.contains(level)) {
                        option { value: level.as_str(), selected: audience == Some(level), "{audience_label(level)}" }
                    }
                }
            }
            if reconfirm {
                p { class: "form-help", role: "status", "Signal review or clearance changed — confirm the audience again before clearing." }
            }
            label { "Rationale"
                textarea { class: "input", required: true, disabled: saving, maxlength: 4000,
                    value: rationale, oninput: move |event| drafts.write().entry(id).or_default().rationale = event.value(),
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
    fn member_review_and_clearance_follow_capabilities() {
        let mut status = api::test_status(contracts::Role::Member, None);
        assert!(!can_review(Some(&status)));
        assert!(clearance_audiences(Some(&status)).is_empty());
        status.capabilities =
            Some(contracts::Capabilities { intel_read: true, intel_review: true, intel_clear: true, ..Default::default() });
        assert!(can_review(Some(&status)));
        assert!(clearance_audiences(Some(&status)).is_empty());
        status.capabilities.as_mut().unwrap().intel_clear_audiences = vec![Audience::CompanyInternal];
        assert_eq!(clearance_audiences(Some(&status)), &[Audience::CompanyInternal]);
        status.capabilities.as_mut().unwrap().intel_clear = false;
        assert!(clearance_audiences(Some(&status)).is_empty());
        status.principal.role = contracts::Role::Operator;
        status.capabilities = None;
        assert!(can_review(Some(&status)));
        assert_eq!(clearance_audiences(Some(&status)), Audience::ALL);
    }

    #[test]
    fn revoked_signals_evict_selection_and_draft() {
        let id = Uuid::new_v4();
        let query = IntelSignalQuery::default();
        let missing = Ok(IntelSignalPage { items: vec![], next_cursor: None });
        let watched = Some((id, query.clone()));
        let filtered = IntelSignalQuery { state: Some(SignalState::New), ..Default::default() };
        assert!(!page_revokes_selection(&filtered, &missing, id, watched.as_ref()));
        for result in [missing, Err(forbidden_or_missing(403)), Err(forbidden_or_missing(404))] {
            let mut selected = Some(id);
            let mut drafts = HashMap::from([(id, ClearanceDraft::default())]);
            assert!(page_revokes_selection(&query, &result, id, watched.as_ref()));
            evict_signal(&mut selected, &mut drafts, id);
            assert_eq!(selected, None);
            assert!(drafts.is_empty());
        }
        for status in [403, 404] {
            let mut selected = Some(id);
            let mut drafts = HashMap::from([(id, ClearanceDraft::default())]);
            if forbidden_or_missing(status).access_lost() {
                evict_signal(&mut selected, &mut drafts, id);
            }
            assert!(selected.is_none() && drafts.is_empty());
        }
        assert!(!forbidden_or_missing(500).access_lost());
    }

    fn forbidden_or_missing(status: u16) -> UiError {
        UiError::from_client(contracts::client::ClientError::Api {
            status,
            error: contracts::ApiError { code: "opaque".into(), message: "test".into(), correlation_id: None },
        })
    }

    #[test]
    fn clearance_denial_discards_rationale_and_audience() {
        let id = Uuid::new_v4();
        for status in [403, 404, 409, 500] {
            let draft =
                ClearanceDraft { rationale: "Sensitive draft".into(), audience: Some(Audience::CompanyInternal), ..Default::default() };
            let mut drafts = HashMap::from([(id, draft)]);
            discard_denied_clearance(&mut drafts, id, &forbidden_or_missing(status));
            assert_eq!(drafts.is_empty(), matches!(status, 403 | 404));
        }
    }

    #[tokio::test]
    async fn agent_revocation_poll_replaces_cached_source_diagnostics() {
        fn app() -> Element {
            let status = use_signal(|| {
                crate::state::Loaded::ready(api::test_status(
                    Role::Agent,
                    Some(contracts::Capabilities { intel_read: true, ..Default::default() }),
                ))
            });
            let mut first = use_signal(|| true);
            let sources = use_resource(move || async move {
                let monitor = *first.peek();
                first.set(false);
                IntelSourceStatus {
                    id: "test.feed".into(),
                    company_id: "test".into(),
                    policy_version: 1,
                    default_audience: Audience::CompanyInternal,
                    last_ingest_at: None,
                    last_event_at: None,
                    last_status: Some(IntelRunStatus::Failed),
                    last_error_code: None,
                    last_error_detail: monitor.then(|| "restricted error".into()),
                    watermark: monitor.then(|| serde_json::json!("restricted watermark")),
                    stale: false,
                    stale_after_secs: 60,
                }
            });
            use_revocation_poll(status, sources);
            let text = match sources.cloned() {
                None => "loading",
                Some(source) if source.last_error_detail.is_some() && source.watermark.is_some() => "restricted error and watermark",
                Some(source) if source.last_error_detail.is_none() && source.watermark.is_none() => "diagnostics redacted",
                Some(_) => "partial redaction",
            };
            rsx! { p { "{text}" } }
        }

        let mut dom = VirtualDom::new(app);
        dom.rebuild_to_vec();
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut saw_diagnostics = false;
            loop {
                dom.wait_for_work().await;
                let edits = format!("{:?}", dom.render_immediate_to_vec());
                saw_diagnostics |= edits.contains("restricted error and watermark");
                if edits.contains("diagnostics redacted") {
                    assert!(saw_diagnostics);
                    break;
                }
            }
        })
        .await
        .expect("non-operator source polling did not replace cached diagnostics");
    }
    use dioxus::{
        dioxus_core::{AttributeValue, Mutation},
        html::{PlatformEventData, SerializedFormData, SerializedHtmlEventConverter, set_event_converter},
    };
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn clearance_draft_survives_an_empty_filter_result_and_reset() {
        check_clearance_remount(|_| {}, 5, false);
    }

    #[test]
    fn clearance_draft_requires_reconfirmation_after_each_review_identity_change() {
        check_clearance_remount(|review| review.revision += 1, 5, true);
        check_clearance_remount(|review| review.policy_version += 1, 5, true);
        check_clearance_remount(|review| review.projection_sha256.push_str("-changed"), 5, true);
    }

    #[test]
    fn concurrent_clearance_requires_reconfirmation_and_submits_latest_generation() {
        check_clearance_remount(|_| {}, 6, true);
    }

    #[test]
    fn clearance_draft_stays_invalidated_after_policy_rollback() {
        type Handles = Rc<Cell<Option<(Signal<IntelSignalQuery>, Signal<IntelSignalReview>)>>>;
        #[derive(Clone, PartialEq, Props)]
        struct TestProps {
            handles: Handles,
            submitted: Rc<Cell<Option<Audience>>>,
        }
        fn app(TestProps { handles, submitted }: TestProps) -> Element {
            let query = use_signal(IntelSignalQuery::default);
            let drafts = use_signal(HashMap::new);
            let review = use_signal(|| IntelSignalReview {
                projection_sha256: "projection".into(),
                policy_version: 1,
                revision: 1,
                first_seen_at: DateTime::UNIX_EPOCH,
                updated_at: DateTime::UNIX_EPOCH,
            });
            handles.set(Some((query, review)));
            rsx! {
                ClearanceForm {
                    id: Uuid::nil(), drafts, review: review.cloned(), source_default_audience: Audience::OperatorOnly,
                    allowed_audiences: Audience::ALL.to_vec(), clearance_generation: 0,
                    saving: false, on_clear: move |input: ClearSignalIn| submitted.set(Some(input.reviewed.audience)),
                }
            }
        }

        set_event_converter(Box::new(SerializedHtmlEventConverter));
        let handles = Rc::new(Cell::new(None));
        let submitted = Rc::new(Cell::new(None));
        let mut dom = VirtualDom::new_with_props(app, TestProps { handles: handles.clone(), submitted: submitted.clone() });
        let initial = dom.rebuild_to_vec();
        let (_, mut review) = handles.get().unwrap();
        let dispatch = |dom: &VirtualDom, edits: &[Mutation], name, value: &str| {
            let mut id = None;
            for mutation in edits {
                match mutation {
                    Mutation::PushId { id: current } => id = Some(*current),
                    Mutation::NewEventListener { name: listener } if listener == name => {
                        let data = PlatformEventData::new(Box::new(SerializedFormData::new(value.into(), vec![])));
                        dom.runtime().handle_event(name, Event::new(Rc::new(data), true).into_any(), id.unwrap());
                        return;
                    }
                    _ => {}
                }
            }
            panic!("No {name} listener in clearance form");
        };
        let audience = Audience::CompanyInternal.as_str();
        let rationale = "Reviewed under policy v1";
        dispatch(&dom, &initial.edits, "change", audience);
        dispatch(&dom, &initial.edits, "input", rationale);
        dom.render_immediate_to_vec();

        // Transition v1 -> v2 (observes mismatch and invalidates stored audience/key).
        dom.in_runtime(|| review.write().policy_version = 2);
        let under_v2 = dom.render_immediate_to_vec();
        assert!(format!("{under_v2:?}").contains("confirm the audience again"));

        // Roll back v2 -> v1 without re-selecting audience: must remain cleared and blocked.
        dom.in_runtime(|| review.write().policy_version = 1);
        dom.render_immediate_to_vec();
        dispatch(&dom, &initial.edits, "submit", "");
        assert_eq!(submitted.get(), None);

        // Re-selecting audience under v1 restores submission and clears reconfirmation state.
        dispatch(&dom, &initial.edits, "change", audience);
        dom.render_immediate_to_vec();
        dispatch(&dom, &initial.edits, "submit", "");
        assert_eq!(submitted.get(), Some(Audience::CompanyInternal));
    }

    fn check_clearance_remount(update_review: impl FnOnce(&mut IntelSignalReview), latest_generation: i64, reconfirm: bool) {
        type Handles = Rc<Cell<Option<(Signal<IntelSignalQuery>, Signal<IntelSignalReview>, Signal<i64>)>>>;
        type Submission = (Audience, Option<i64>);
        #[derive(Clone, PartialEq, Props)]
        struct TestProps {
            handles: Handles,
            submitted: Rc<Cell<Option<Submission>>>,
        }
        fn app(TestProps { handles, submitted }: TestProps) -> Element {
            let query = use_signal(IntelSignalQuery::default);
            let drafts = use_signal(HashMap::new);
            let review = use_signal(|| IntelSignalReview {
                projection_sha256: "projection".into(),
                policy_version: 1,
                revision: 1,
                first_seen_at: DateTime::UNIX_EPOCH,
                updated_at: DateTime::UNIX_EPOCH,
            });
            let clearance_generation = use_signal(|| 5);
            handles.set(Some((query, review, clearance_generation)));
            rsx! {
                if has_signal_filters(&query.read()) {
                    p { "{empty_signal_title(&query.read())}" }
                } else {
                    ClearanceForm {
                        id: Uuid::nil(), drafts, review: review.cloned(), source_default_audience: Audience::OperatorOnly,
                        allowed_audiences: vec![Audience::CompanyInternal], clearance_generation: clearance_generation(),
                        saving: false, on_clear: move |input: ClearSignalIn| {
                            submitted.set(Some((input.reviewed.audience, input.expected_clearance_generation)));
                        },
                    }
                }
            }
        }

        set_event_converter(Box::new(SerializedHtmlEventConverter));
        let handles = Rc::new(Cell::new(None));
        let submitted = Rc::new(Cell::new(None));
        let mut dom = VirtualDom::new_with_props(app, TestProps { handles: handles.clone(), submitted: submitted.clone() });
        let initial = dom.rebuild_to_vec();
        let (mut query, mut review, mut clearance_generation) = handles.get().unwrap();
        let dispatch = |dom: &VirtualDom, edits: &[Mutation], name, value: &str| {
            let mut id = None;
            for mutation in edits {
                match mutation {
                    Mutation::PushId { id: current } => id = Some(*current),
                    Mutation::NewEventListener { name: listener } if listener == name => {
                        let data = PlatformEventData::new(Box::new(SerializedFormData::new(value.into(), vec![])));
                        dom.runtime().handle_event(name, Event::new(Rc::new(data), true).into_any(), id.unwrap());
                        return;
                    }
                    _ => {}
                }
            }
            panic!("No {name} listener in clearance form");
        };
        let audience = Audience::CompanyInternal.as_str();
        let rationale = "Reviewed the source evidence";
        assert!(!format!("{initial:?}").contains(Audience::PublicCleared.as_str()));
        dispatch(&dom, &initial.edits, "input", rationale);
        dispatch(&dom, &initial.edits, "change", Audience::PublicCleared.as_str());
        dispatch(&dom, &initial.edits, "submit", "");
        assert_eq!(submitted.get(), None);
        dispatch(&dom, &initial.edits, "change", audience);
        let typed = format!("{:?}", dom.render_immediate_to_vec());
        assert!(typed.contains(audience) && typed.contains(rationale));

        dom.in_runtime(|| query.write().state = Some(SignalState::Acknowledged));
        let empty = format!("{:?}", dom.render_immediate_to_vec());
        assert!(empty.contains("No matching signals."));
        dom.in_runtime(|| {
            update_review(&mut review.write());
            clearance_generation.set(latest_generation);
            query.set(IntelSignalQuery::default());
        });
        let reset = dom.render_immediate_to_vec();
        let values: Vec<_> = reset
            .edits
            .iter()
            .filter_map(|mutation| match mutation {
                Mutation::SetAttribute { name, value: AttributeValue::Text(value), .. } if name == "value" => Some(value.as_str()),
                _ => None,
            })
            .collect();
        let expected = if reconfirm { "" } else { audience };
        assert_eq!(values.first(), Some(&expected)); // Select value, before its option values.
        assert_eq!(values.last(), Some(&rationale)); // Textarea value.
        assert_eq!(format!("{reset:?}").contains("confirm the audience again"), reconfirm);
        let selected: Vec<_> = reset
            .edits
            .iter()
            .filter_map(|mutation| match mutation {
                Mutation::SetAttribute { name, value: AttributeValue::Bool(selected), .. } if name == "selected" => Some(*selected),
                _ => None,
            })
            .collect();
        assert_eq!(selected, [reconfirm, !reconfirm]);
        let expected_submission = Some((Audience::CompanyInternal, Some(latest_generation)));
        dispatch(&dom, &reset.edits, "submit", "");
        assert_eq!(submitted.get(), if reconfirm { None } else { expected_submission });
        if reconfirm {
            dispatch(&dom, &reset.edits, "change", audience);
            dom.render_immediate_to_vec();
            dispatch(&dom, &reset.edits, "submit", "");
            assert_eq!(submitted.get(), expected_submission);
        }
    }

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
