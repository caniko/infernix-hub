//! Provider workspace. Only the mounted view reads provider data; activity
//! resources belong to their individual tabs and never join the shell poll.
mod accounts;
mod activity;
mod charts;
mod dashboard;
mod data_settings;
mod routing;

use std::time::Duration;

use chrono::{DateTime, Utc};
use contracts::{Role, SystemStatus, client::HubClient, providers::*};
use dioxus::core::{Runtime, current_scope_id};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api::{timed, timed_for},
    components::{Chip, EmptyState, ErrorCard, Loading},
    format::{fmt_count, fmt_dt, fmt_signed_count},
    state::{Loaded, Store, UiError},
};

#[derive(Clone, Copy)]
struct PanelState {
    settings: Signal<ProviderPanelSettings>,
    saved_settings: Signal<Option<ProviderPanelSettings>>,
    toolbar_save_pending: Signal<bool>,
    toolbar_saving: Signal<bool>,
    epoch: Signal<u64>,
    busy: Signal<bool>,
    notice: Signal<Option<(bool, String)>>,
    oauth: Signal<Option<OAuthLoginState>>,
    operation: Signal<Option<OperationProgress>>,
    status: Signal<Loaded<SystemStatus>>,
    scope: ScopeId,
}

#[derive(Clone, PartialEq)]
struct OperationProgress {
    id: Uuid,
    kind: String,
    status: OperationStatus,
    output: Option<String>,
    detail: Option<String>,
}

impl PanelState {
    fn changed(mut self) {
        self.epoch += 1;
    }

    fn report(mut self, result: Result<String, UiError>) {
        self.notice.set(Some(match result {
            Ok(text) => (true, text),
            Err(e) => (false, format!("{} — {}", e.code, e.message)),
        }));
    }

    fn begin(self) -> Option<Busy> {
        if !self.operator() {
            return None;
        }
        let mut busy = self.busy;
        let mut flag = busy.try_write().ok()?;
        if *flag {
            return None;
        }
        *flag = true;
        Some(Busy(self.busy))
    }

    fn operator(self) -> bool {
        let status = self.status.read();
        status.error.is_none() && status.data.as_ref().is_some_and(|s| s.principal.role == Role::Operator)
    }
}

// A child tab may unmount while a request is in flight. Cancellation must
// release the panel-wide write lock, even if the pending future never ran.
struct Busy(Signal<bool>);

impl Drop for Busy {
    fn drop(&mut self) {
        if let Ok(mut flag) = self.0.try_write() {
            *flag = false;
        }
    }
}

const TABS: [&str; 6] = ["Dashboard", "Accounts", "Activity", "Routing & Proxy", "Data & Export", "Settings & Help"];
const SORTS: &[(&str, &str)] = &[("reset", "Soonest reset"), ("left", "Most left"), ("name", "Name")];

fn sort_key(sort: AccountSortOrder) -> &'static str {
    match sort {
        AccountSortOrder::SoonestReset => "reset",
        AccountSortOrder::MostLeft => "left",
        AccountSortOrder::Name => "name",
    }
}

fn sort_order(key: &str) -> AccountSortOrder {
    match key {
        "left" => AccountSortOrder::MostLeft,
        "name" => AccountSortOrder::Name,
        _ => AccountSortOrder::SoonestReset,
    }
}

fn operation_status_label(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Pending => "Pending",
        OperationStatus::Dispatched => "Dispatched",
        OperationStatus::AwaitingOperator => "Awaiting operator",
        OperationStatus::Succeeded => "Succeeded",
        OperationStatus::Failed => "Failed",
        OperationStatus::Ambiguous => "Ambiguous",
        OperationStatus::Cancelled => "Cancelled",
    }
}

fn enrollment_state_label(state: EnrollmentState) -> &'static str {
    match state {
        EnrollmentState::Catalogued => "Catalogued",
        EnrollmentState::Deployed => "Deployed",
        EnrollmentState::Selected => "Selected",
        EnrollmentState::Verified => "Verified",
        EnrollmentState::Unobserved => "Unobserved",
        EnrollmentState::Error => "Error",
    }
}

fn quota_status_label(status: QuotaStatus) -> Option<&'static str> {
    match status {
        QuotaStatus::Allowed => Some("Allowed"),
        QuotaStatus::Warning => Some("Warning"),
        QuotaStatus::Rejected => Some("Rejected"),
        QuotaStatus::Unknown => None,
    }
}

fn quota_unit_label(unit: QuotaUnit) -> &'static str {
    match unit {
        QuotaUnit::Percent => "%",
        QuotaUnit::Credits => "credits",
        QuotaUnit::Currency => "currency",
        QuotaUnit::Requests => "requests",
        QuotaUnit::Tokens => "tokens",
    }
}

fn reset_cadence_label(cadence: ResetCadence) -> Option<&'static str> {
    match cadence {
        ResetCadence::Fixed => Some("Fixed reset"),
        ResetCadence::RollingOnUse => Some("Rolling on use"),
        ResetCadence::Unknown => None,
    }
}

fn client_wiring_label(state: ClientWiringState) -> &'static str {
    match state {
        ClientWiringState::Applied => "Applied",
        ClientWiringState::Outdated => "Outdated",
        ClientWiringState::NotApplied => "Not applied",
        ClientWiringState::ExternallyManaged => "Externally managed",
        ClientWiringState::Error => "Error",
    }
}

fn route_strategy_label(strategy: RouteStrategy) -> &'static str {
    match strategy {
        RouteStrategy::SoonestReset => "Soonest Reset",
        RouteStrategy::FillFirst => "Fill First",
        RouteStrategy::RoundRobin => "Round Robin",
        RouteStrategy::WeightedRoundRobin => "Weighted Round Robin",
    }
}

fn api_key_order_label(order: ApiKeyOrder) -> &'static str {
    match order {
        ApiKeyOrder::KeysLast => "Keys last",
        ApiKeyOrder::KeysFirst => "Keys first",
        ApiKeyOrder::SubscriptionsOnly => "Subscriptions only",
    }
}

fn request_identity_label(mode: RequestIdentityMode) -> &'static str {
    match mode {
        RequestIdentityMode::Api => "API identity",
        RequestIdentityMode::AutoClient => "Automatic client identity",
    }
}

fn activity_source_label(source: ActivitySource) -> &'static str {
    match source {
        ActivitySource::Proxy => "Proxy",
        ActivitySource::LocalImport => "Local import",
        ActivitySource::All => "All",
    }
}

fn activity_purpose_label(purpose: ActivityPurpose) -> &'static str {
    match purpose {
        ActivityPurpose::Ordinary => "Ordinary",
        ActivityPurpose::WindowStarter => "Window starter",
        ActivityPurpose::Canary => "Canary",
        ActivityPurpose::ThinkingTest => "Thinking test",
        ActivityPurpose::EvalReplay => "Eval replay",
    }
}

fn capture_state_label(state: CaptureState) -> &'static str {
    match state {
        CaptureState::None => "None",
        CaptureState::MetadataOnly => "Metadata only",
        CaptureState::Transformed => "Transformed",
        CaptureState::RawWire => "Raw wire",
        CaptureState::Truncated => "Truncated",
        CaptureState::Evicted => "Evicted",
        CaptureState::Dropped => "Dropped",
        CaptureState::Rejected => "Rejected",
    }
}

fn granularity_label(granularity: TotalsGranularity) -> &'static str {
    match granularity {
        TotalsGranularity::Hour => "hourly",
        TotalsGranularity::Day => "daily",
        TotalsGranularity::Week => "weekly",
        TotalsGranularity::Month => "monthly",
    }
}

fn eval_predicate_label(kind: EvalPredicateKind) -> &'static str {
    match kind {
        EvalPredicateKind::Contains => "Contains",
        EvalPredicateKind::NotContains => "Does not contain",
        EvalPredicateKind::Equals => "Equals",
    }
}

#[component]
pub fn ProvidersView() -> Element {
    let client = use_context::<HubClient>();
    let store = use_context::<Store>();
    let scope = current_scope_id();
    let mut panel = use_context_provider(|| PanelState {
        settings: Signal::new(ProviderPanelSettings::default()),
        saved_settings: Signal::new(None),
        toolbar_save_pending: Signal::new(false),
        toolbar_saving: Signal::new(false),
        epoch: Signal::new(0),
        busy: Signal::new(false),
        notice: Signal::new(None),
        oauth: Signal::new(None),
        operation: Signal::new(None),
        status: store.status,
        scope,
    });
    let mut selected = use_signal(String::new);
    let mut tab = use_signal(|| 0usize);
    let mut providers = use_signal(Loaded::<Vec<ProviderSummary>>::default);
    let mut settings_ready = use_signal(|| false);
    let mut settings_error = use_signal(|| None::<UiError>);
    let preferences_client = client.clone();
    let mut preferences = use_resource(move || {
        let client = preferences_client.clone();
        async move {
            match timed(client.provider_settings()).await {
                Ok(settings) => {
                    panel.settings.set(settings.clone());
                    panel.saved_settings.set(Some(settings));
                    settings_ready.set(true);
                    settings_error.set(None);
                }
                Err(error) => settings_error.set(Some(error)),
            }
        }
    });
    let polling_client = client.clone();
    use_resource(move || {
        let _ = (panel.epoch)();
        let client = polling_client.clone();
        async move {
            loop {
                match timed(client.providers()).await {
                    Ok(rows) => providers.set(Loaded::ready(rows)),
                    Err(error) => providers.write().failed(error),
                }
                let cadence = panel.settings.peek().refresh_interval_secs.clamp(5, 3600);
                tokio::time::sleep(Duration::from_secs(cadence.into())).await;
            }
        }
    });
    let loaded = providers.cloned();
    let inventory_stale = loaded.error.is_some();
    let all = loaded.data.unwrap_or_default();
    let selection = selected.cloned();
    let scoped: Vec<_> = all.iter().filter(|row| selection.is_empty() || row.descriptor.id == selection).cloned().collect();
    let section = tab();
    let show_provider_filters = section < 4;
    let view_key = if section == 2 { "" } else { &selection };
    let settings = panel.settings.cloned();
    let mask_client = client.clone();
    let sort_client = client.clone();
    rsx! {
        main { class: "work providers-work stack",
            header { class: "glass-card providers-toolbar stack", style: "padding: 8px 12px; gap: 6px;",
                nav { class: "tab-bar", style: "padding-top: 0; border-top: 0;", aria_label: "AI Usage sections",
                    for (i, label) in TABS.into_iter().enumerate() {
                        button { key: "{label}", class: if section == i { "tab-btn active" } else { "tab-btn" },
                            aria_current: if section == i { "page" } else { "false" }, onclick: move |_| tab.set(i), "{label}" }
                    }
                }
                if show_provider_filters {
                    div { class: "provider-bar", role: "group", aria_label: "Provider filter",
                        button { class: "filter-chip", aria_pressed: selection.is_empty(), onclick: move |_| selected.set(String::new()), "All" }
                        for provider in &all {
                            {
                                let id = provider.descriptor.id.clone();
                                rsx! { button {
                                    key: "{id}", class: "filter-chip", aria_pressed: id == selection,
                                    onclick: move |_| selected.set(id.clone()), "{provider.descriptor.label}"
                                } }
                            }
                        }
                    }
                }
                if section < 5 {
                    div { class: "row",
                        if section < 2 {
                            Choice { label: "Account sort", value: sort_key(settings.account_sort), options: pairs(SORTS),
                                change: move |value: String| {
                                    panel.settings.write().account_sort = sort_order(&value);
                                    if panel.operator() && settings_ready() {
                                        schedule_toolbar_settings_save(panel, sort_client.clone());
                                    }
                                } }
                        }
                        if show_provider_filters {
                            Check { label: "Mask emails", value: settings.mask_emails, disabled: !settings_ready(),
                                change: move |value| {
                                    panel.settings.write().mask_emails = value;
                                    if panel.operator() && settings_ready() {
                                        schedule_toolbar_settings_save(panel, mask_client.clone());
                                    }
                                } }
                        }
                        if section != 2 {
                            button { class: "btn secondary", title: "Reload provider data from the hub", onclick: move |_| panel.changed(), "Reload" }
                        }
                    }
                }
            }
            if let Some(error) = loaded.error {
                ErrorCard { code: error.code, message: error.message }
                if !all.is_empty() { p { class: "monitoring-error", role: "status", "Stale provider inventory — showing the last successful read." } }
            }
            if let Some(error) = settings_error() {
                ErrorCard { code: error.code, message: format!("Panel preferences unavailable: {}", error.message) }
                button { class: "btn btn-glass", onclick: move |_| preferences.restart(), "Retry panel preferences" }
            }
            if !panel.operator() {
                p { class: "muted", "Read-only until the hub confirms an operator session." }
            }
            if let Some((ok, message)) = (panel.notice)() {
                div { class: if ok { "notice-bar ok" } else { "notice-bar err" }, role: "status",
                    span { "{message}" }
                    button { class: "icon-btn", aria_label: "Dismiss", onclick: move |_| panel.notice.set(None), "✕" }
                }
            }
            if let Some(login) = (panel.oauth)() {
                section { class: "glass-card stack", aria_label: "OAuth login",
                    h2 { "OAuth · {login.state}" }
                    p { "Session {login.session_id} · expires {date(login.expires_at)}" }
                    if let Some(message) = login.message { p { "{message}" } }
                    if let Some(url) = login.authorize_url {
                        Output { label: "Authorization link — open in your browser", value: url }
                    }
                    button { class: "btn btn-glass", onclick: move |_| panel.oauth.set(None), "Dismiss login instructions" }
                }
            }
            if let Some(op) = (panel.operation)() {
                div { class: "provider-inset stack",
                    p { class: "mono", "{op.kind} · {operation_status_label(op.status)} · {op.id}" }
                    if let Some(detail) = &op.detail { p { "{detail}" } }
                    if let Some(value) = &op.output {
                        Output { label: "Credential export receipt (redacted by the hub)", value: value.clone() }
                    }
                    div { class: "row",
                        if matches!(op.status, OperationStatus::Pending | OperationStatus::Dispatched | OperationStatus::AwaitingOperator | OperationStatus::Ambiguous) {
                            button { class: "btn btn-glass", disabled: (panel.busy)(), onclick: move |_| {
                                let Some(busy) = panel.begin() else { return; };
                                let client = client.clone();
                                spawn(async move {
                                    let _busy = busy;
                                    match timed(client.provider_operation(op.id)).await {
                                        Ok(current) => { record_operation(panel, &current); panel.changed(); }
                                        Err(error) => panel.report(Err(error)),
                                    }
                                });
                            }, "Check operation status" }
                        }
                        button { class: "btn btn-glass", disabled: (panel.busy)(), onclick: move |_| panel.operation.set(None), "Dismiss receipt" }
                    }
                }
            }
            if show_provider_filters {
                for provider in scoped.iter().filter(|row| row.descriptor.health == ProviderHealth::Offline) {
                    p { key: "{provider.descriptor.id}", class: "monitoring-error", role: "status",
                        {format!("{} offline — {}", provider.descriptor.label, provider.descriptor.status_message.as_deref().unwrap_or("last observation only"))}
                    }
                }
                if loaded.loading { Loading { text: "Loading model providers…".to_string() } }
            }
            if !loaded.loading && scoped.is_empty() && !inventory_stale && matches!(section, 0 | 1 | 3) {
                EmptyState { title: "No providers in this view.".to_string(), hint: Some("Select All or register a provider with the hub.".into()) }
            }
            // Activity keys its results separately so provider changes retain the other filters.
            {rsx! { div { key: "{view_key}", class: "stack",
                match section {
                    0 => rsx! { dashboard::Dashboard { providers: scoped, stale: inventory_stale, all_providers: selection.is_empty() } },
                    1 => rsx! { accounts::Accounts { providers: scoped } },
                    2 => rsx! { activity::Activity { providers: all, selected } },
                    3 => rsx! { routing::Routing { providers: scoped } },
                    4 => rsx! { data_settings::DataExport {} },
                    _ => rsx! { data_settings::SettingsHelp { providers: all, ready: settings_ready() } },
                }
            } }}
        }
    }
}

fn schedule_toolbar_settings_save(mut panel: PanelState, client: HubClient) {
    panel.toolbar_save_pending.set(true);
    let Ok(mut saving) = panel.toolbar_saving.try_write() else {
        return;
    };
    if *saving {
        return;
    }
    *saving = true;
    drop(saving);
    Runtime::current().spawn(panel.scope, async move {
        let _guard = Busy(panel.toolbar_saving);
        while *panel.toolbar_save_pending.peek() {
            while *panel.busy.peek() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !*panel.toolbar_save_pending.peek() {
                break;
            }
            panel.toolbar_save_pending.set(false);
            let current = panel.settings.peek().clone();
            let mut updated = panel.saved_settings.peek().clone().unwrap_or_else(|| current.clone());
            updated.mask_emails = current.mask_emails;
            updated.account_sort = current.account_sort;
            match timed(client.set_provider_settings(&updated)).await {
                Ok(saved) => panel.saved_settings.set(Some(saved)),
                Err(error) => {
                    panel.report(Err(error));
                    break;
                }
            }
        }
    });
}

fn supports(provider: &ProviderSummary, account: Option<&ModelAccount>, cap: ProviderCapability) -> bool {
    provider.descriptor.health != ProviderHealth::Offline
        && provider.descriptor.capabilities.contains(&cap)
        && account.is_none_or(|a| a.capabilities.contains(&cap))
}

#[component]
fn OperationButton(
    provider: ProviderSummary,
    #[props(optional)] account: Option<ModelAccount>,
    command: ProviderCommand,
    label: &'static str,
    #[props(optional)] revision: Option<i64>,
    #[props(default)] confirm: bool,
    #[props(default)] disabled: bool,
    #[props(optional)] tone: Option<&'static str>,
) -> Element {
    let panel = use_context::<PanelState>();
    let client = use_context::<HubClient>();
    let mut armed = use_signal(|| None::<ProviderCommand>);
    if !panel.operator() || !supports(&provider, account.as_ref(), command.required_capability()) {
        return rsx! {};
    }
    let confirming = armed.read().as_ref() == Some(&command);
    let tone = tone.unwrap_or(if confirming { "danger" } else { "secondary" });
    rsx! {
        span { class: "row",
            button {
                class: "btn {tone}",
                disabled: disabled || (panel.busy)(),
                onclick: move |_| {
                    if confirm && armed.peek().as_ref() != Some(&command) {
                        armed.set(Some(command.clone()));
                        return;
                    }
                    let Some(busy) = panel.begin() else { return; };
                    armed.set(None);
                    let (client, id, account, command) = (client.clone(), provider.descriptor.id.clone(), account.clone(), command.clone());
                    Runtime::current().spawn(panel.scope, async move {
                        let _busy = busy;
                        execute_operation(panel, &client, &id, account.as_ref(), revision, command).await;
                    });
                },
                if confirming { "Confirm: {label}" } else { "{label}" }
            }
            if confirming {
                button { class: "btn ghost", onclick: move |_| armed.set(None), "Cancel" }
            }
        }
    }
}

async fn execute_operation(
    mut panel: PanelState,
    client: &HubClient,
    id: &str,
    account: Option<&ModelAccount>,
    revision: Option<i64>,
    command: ProviderCommand,
) {
    if matches!(&command, ProviderCommand::Credential(CredentialCommand::BeginOAuth { .. })) {
        panel.oauth.set(None);
    }
    panel.operation.set(None);
    let input = SubmitOperationIn {
        idempotency_key: Uuid::new_v4().to_string(),
        expected_revision: revision,
        target_account_id: account.map(|a| a.id.clone()),
        target_host: account.and_then(|a| a.host.clone()),
        target_user: account.and_then(|a| a.unix_user.clone()),
        command,
    };
    let result = async {
        let id = segment(id).ok_or_else(|| UiError::new("provider_id", "Invalid provider id."))?;
        let mut op = timed_for(Duration::from_secs(20), client.submit_provider_operation(&id, &input)).await?;
        for _ in 0..8 {
            record_operation(panel, &op);
            if !matches!(op.status, OperationStatus::Pending | OperationStatus::Dispatched) {
                break;
            }
            panel.report(Ok(format!("{} · {} · {}", op.command_kind, operation_status_label(op.status), op.id)));
            tokio::time::sleep(Duration::from_secs(2)).await;
            op = timed(client.provider_operation(op.id)).await?;
        }
        record_operation(panel, &op);
        if matches!(op.status, OperationStatus::Failed | OperationStatus::Ambiguous | OperationStatus::Cancelled) {
            Err(UiError::new(
                "operation",
                format!("{} · {} · {}. {}", op.command_kind, operation_status_label(op.status), op.id, op.error.unwrap_or_default()),
            ))
        } else {
            Ok(format!("{} · {} · {}", op.command_kind, operation_status_label(op.status), op.id))
        }
    }
    .await;
    panel.report(result);
    panel.changed();
}

fn record_operation(mut panel: PanelState, op: &ProviderOperation) {
    let receipt = op.receipt.as_ref();
    let message = |key| receipt.and_then(|r| r.get(key)).and_then(serde_json::Value::as_str).map(str::to_string);
    if matches!(&op.command, ProviderCommand::Credential(CredentialCommand::BeginOAuth { .. })) {
        let session = receipt.map(|r| r.get("oauth").unwrap_or(r)).and_then(|r| {
            let mut state = r.as_object()?.clone();
            state.entry("provider_id").or_insert_with(|| serde_json::Value::String(op.provider_id.clone()));
            let mut state: OAuthLoginState = serde_json::from_value(serde_json::Value::Object(state)).ok()?;
            if state.message.is_none() {
                state.message = message("message");
            }
            Some(state)
        });
        if session.is_some() {
            panel.oauth.set(session);
        }
    }
    panel.operation.set(Some(OperationProgress {
        id: op.id,
        kind: op.command_kind.clone(),
        status: op.status,
        output: matches!(&op.command, ProviderCommand::Credential(CredentialCommand::ExportLogin { .. }))
            .then(|| receipt.map(pretty))
            .flatten(),
        detail: op.error.clone().or_else(|| message("message")).or_else(|| message("reason")),
    }));
}

// HubClient accepts path segments, not arbitrary URLs. Escape path grammar
// before sending ids supplied by a remote adapter.
fn segment(id: &str) -> Option<String> {
    if matches!(id, "" | "." | "..") {
        return None;
    }
    Some(
        id.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(b).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect(),
    )
}

fn path_id(id: Option<String>) -> Result<String, UiError> {
    id.ok_or_else(|| UiError::new("invalid_id", "Invalid provider or session id."))
}

fn read_view<T>(result: Option<&Result<T, UiError>>, render: impl FnOnce(&T) -> Element) -> Element {
    match result {
        Some(Ok(data)) => render(data),
        Some(Err(err)) => rsx! { ErrorCard { code: err.code.clone(), message: err.message.clone() } },
        None => rsx! { Loading { text: "Loading…".to_string() } },
    }
}

fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn date(value: Option<DateTime<Utc>>) -> String {
    value.map(fmt_dt).unwrap_or_else(|| "—".into())
}

fn number(value: Option<f64>) -> String {
    value.filter(|v| v.is_finite()).map_or_else(|| "—".into(), |v| format!("{v:.2}"))
}

fn pairs(options: &[(&str, &str)]) -> Vec<(String, String)> {
    options.iter().map(|(key, label)| (key.to_string(), label.to_string())).collect()
}

#[component]
fn Choice(
    label: &'static str,
    #[props(into)] value: String,
    options: Vec<(String, String)>,
    #[props(optional)] description_id: Option<String>,
    change: EventHandler<String>,
) -> Element {
    rsx! { label { class: "provider-field", span { "{label}" }
        select { class: "input", value: "{value}", aria_describedby: description_id, onchange: move |e| change.call(e.value()),
            for (key, text) in options { option { key: "{key}", value: "{key}", "{text}" } }
        }
    } }
}

#[component]
fn Field(
    label: &'static str,
    #[props(into)] value: String,
    #[props(default = "text")] kind: &'static str,
    #[props(optional)] description_id: Option<String>,
    change: EventHandler<String>,
) -> Element {
    rsx! { label { class: "provider-field", span { "{label}" }
        input { class: "input", r#type: kind, value: "{value}", aria_describedby: description_id, oninput: move |e| change.call(e.value()) }
    } }
}

#[component]
fn Check(
    label: &'static str,
    value: bool,
    #[props(default)] disabled: bool,
    #[props(optional)] description_id: Option<String>,
    change: EventHandler<bool>,
) -> Element {
    rsx! { label { class: "row provider-check",
        input { r#type: "checkbox", checked: value, disabled, aria_describedby: description_id, onchange: move |e| change.call(e.checked()) }
        "{label}"
    } }
}

#[component]
fn Table(label: &'static str, headers: &'static [&'static str], rows: Vec<Vec<String>>) -> Element {
    rsx! {
        if rows.is_empty() {
            p { class: "muted", "No rows for {label}." }
        } else {
            div { class: "provider-table", tabindex: "0", role: "region", aria_label: label,
                table {
                    caption { class: "sr-only", "{label}" }
                    thead { tr { for header in headers { th { scope: "col", "{header}" } } } }
                    tbody { for row in rows { tr { for value in row { td { "{value}" } } } } }
                }
            }
        }
    }
}

#[component]
fn Output(label: &'static str, value: String, #[props(optional)] filename: Option<&'static str>) -> Element {
    let mut status = use_signal(String::new);
    let content = value.clone();
    rsx! {
        label { class: "provider-field", "{label}", textarea { class: "input mono", readonly: true, value: "{value}" } }
        div { class: "row",
            button { class: "btn btn-glass", onclick: move |_| {
                let value = content.clone();
                spawn(async move {
                    let eval = document::eval("const text = await dioxus.recv(); await navigator.clipboard.writeText(text); return true;");
                    let copied = eval.send(value).is_ok() && eval.join::<bool>().await.unwrap_or(false);
                    status.set(if copied { "Copied." } else { "Clipboard unavailable; select the text to copy." }.into());
                });
            }, "Copy" }
            if let Some(name) = filename {
                a { class: "btn btn-glass", download: name, href: crate::api::data_uri("application/octet-stream", value.as_bytes()), "Download" }
            }
            span { role: "status", "{status}" }
        }
    }
}

fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "Could not format response.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_sort_keeps_the_active_login_first() {
        let provider: ProviderSummary = serde_json::from_value(serde_json::json!({
            "descriptor": {
                "id": "test", "label": "Test", "family": "test",
                "mode": "fixture", "verification": "built"
            },
            "account_count": 3, "enabled_accounts": 3, "pools": [],
            "accounts": [
                {"id": "z", "provider_id": "test", "source_account_id": "z", "display_name": "Zulu", "is_active": true},
                {"id": "b", "provider_id": "test", "source_account_id": "b", "display_name": "Bravo"},
                {"id": "a", "provider_id": "test", "source_account_id": "a", "display_name": "Alpha", "is_next_pick": true}
            ]
        }))
        .unwrap();
        let rows = dashboard::sorted_accounts(&provider, AccountSortOrder::Name, false);
        assert_eq!(rows.iter().map(|account| account.id.as_str()).collect::<Vec<_>>(), ["z", "a", "b"]);
    }

    #[test]
    fn unknown_read_only_provider_renders_without_quota_controls() {
        fn app() -> Element {
            use_context_provider(|| HubClient::new("http://127.0.0.1:1", ""));
            use_context_provider(|| {
                let status = SystemStatus {
                    api_version: "v1".into(),
                    fixture_mode: true,
                    production_paused: false,
                    integrations: vec![],
                    feeds: vec![],
                    principal: contracts::Principal { id: Uuid::nil(), name: "test".into(), role: Role::Operator },
                };
                PanelState {
                    settings: Signal::new(ProviderPanelSettings::default()),
                    saved_settings: Signal::new(None),
                    toolbar_save_pending: Signal::new(false),
                    toolbar_saving: Signal::new(false),
                    epoch: Signal::new(0),
                    busy: Signal::new(false),
                    notice: Signal::new(None),
                    oauth: Signal::new(None),
                    operation: Signal::new(None),
                    status: Signal::new(Loaded::ready(status)),
                    scope: current_scope_id(),
                }
            });
            let provider = ProviderSummary {
                descriptor: ProviderDescriptor {
                    id: "future-provider".into(),
                    label: "Future provider".into(),
                    family: "unrecognized".into(),
                    mode: contracts::AdapterMode::Fixture,
                    verification: contracts::Verification::Built,
                    evidence_kinds: Default::default(),
                    capabilities: Default::default(),
                    router_id: None,
                    health: ProviderHealth::Unobserved,
                    status_message: None,
                    last_observed_at: None,
                },
                accounts: vec![],
                account_count: 0,
                enabled_accounts: 0,
                pools: vec![],
                active_account_id: None,
                preferred_account_id: None,
                next_pick_account_id: None,
            };
            rsx! {
                dashboard::Dashboard { providers: vec![provider.clone()], stale: false, all_providers: true }
                OperationButton { provider, label: "Must not render",
                    command: ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id: None }) }
            }
        }
        let output = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(output.contains("Future provider"));
        assert!(output.contains("Not observed"));
        assert!(output.contains("Future provider · No quota reading · Not observed"));
        assert!(!output.contains("Must not render"));
    }

    #[test]
    fn remote_ids_cannot_change_request_path() {
        assert_eq!(segment("custom.host:pool/one?token#frag").as_deref(), Some("custom.host%3Apool%2Fone%3Ftoken%23frag"));
        assert_eq!(segment(".."), None);
        assert_eq!(segment("%2e%2e"), Some("%252e%252e".into()));
    }
}
