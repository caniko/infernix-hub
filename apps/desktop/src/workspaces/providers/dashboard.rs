use std::{cmp::Ordering, collections::BTreeMap};

use chrono::TimeDelta;

use super::{charts::*, *};

pub(super) const LOOKBACK: &[(&str, &str)] = &[("3", "3h"), ("6", "6h"), ("12", "12h"), ("24", "24h")];
pub(super) const HORIZON: &[(&str, &str)] = &[("6", "6h"), ("12", "12h"), ("24", "24h"), ("48", "48h")];

#[component]
pub(super) fn Dashboard(providers: Vec<ProviderSummary>, stale: bool, all_providers: bool) -> Element {
    rsx! {
        if all_providers && !providers.is_empty() {
            div { class: "row", role: "group", aria_label: "Provider quota summary",
                for provider in &providers {
                    {
                        let quota = pool_quota_summary(&provider.pools);
                        let (health, tone) = provider_health(provider.descriptor.health, stale);
                        rsx! {
                            span { key: "{provider.descriptor.id}", class: "chip provider-pill {tone}",
                                style: "white-space: normal; overflow-wrap: anywhere;",
                                "{provider.descriptor.label} · {quota} · {health}"
                            }
                        }
                    }
                }
            }
        }
        div { class: "provider-grid",
            for provider in providers {
                ProviderCard { key: "{provider.descriptor.id}", provider, stale }
            }
        }
    }
}

fn is_extra(kind: QuotaWindowKind) -> bool {
    matches!(kind, QuotaWindowKind::ModelScoped | QuotaWindowKind::Spend)
}

fn provider_health(health: ProviderHealth, stale: bool) -> (&'static str, &'static str) {
    match health {
        ProviderHealth::Healthy if stale => ("Healthy (last known)", "muted"),
        ProviderHealth::Healthy => ("Healthy", "ok"),
        ProviderHealth::Degraded => ("Degraded", "warn"),
        ProviderHealth::Offline => ("Offline", "bad"),
        ProviderHealth::Unobserved => ("Not observed", "muted"),
    }
}

fn pool_quota_summary(pools: &[PoolWindowSummary]) -> String {
    pools
        .iter()
        .filter(|pool| !is_extra(pool.kind))
        .filter_map(|pool| {
            let (left, _) = quota_values(QuotaObservationState::Measured, pool.weighted_remaining_percent, pool.weighted_used_percent);
            left.map(|left| (pool, left))
        })
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map_or_else(
            || "No quota reading".into(),
            |(pool, left)| {
                let estimate = if pool.is_estimate { "≈" } else { "" };
                format!("{estimate}{left:.1}% left ({})", window_label(&pool.label))
            },
        )
}

pub(super) fn is_next_pick(provider: &ProviderSummary, account: &ModelAccount) -> bool {
    let reported = account.is_next_pick || provider.next_pick_account_id.as_ref() == Some(&account.id);
    reported && account_status(account, true) == AccountStatus::NextPick
}

fn account_status(account: &ModelAccount, is_next_pick: bool) -> AccountStatus {
    AccountStatus::resolve_with_error(
        account.enabled,
        account.cooldown_until,
        Utc::now(),
        is_next_pick,
        account.enrollment_state == EnrollmentState::Error || (account.last_error.is_none() && account.status == AccountStatus::Error),
        account.last_error.as_ref(),
    )
}

pub(super) fn account_name(account: &ModelAccount, mask: bool) -> String {
    if !mask {
        return account.display_name.clone();
    }
    if account.display_name.contains('@') { mask_email(&account.display_name) } else { account.display_name.clone() }
}

pub(super) fn sorted_accounts(provider: &ProviderSummary, sort: AccountSortOrder, mask: bool) -> Vec<&ModelAccount> {
    let now = Utc::now();
    let next_reset = |a: &ModelAccount| a.windows.iter().filter_map(|w| w.resets_at.filter(|ts| *ts > now)).min();
    let most_left = |a: &ModelAccount| {
        a.windows
            .iter()
            .filter(|w| !is_extra(w.kind) && !w.stale && w.observation_state == QuotaObservationState::Measured)
            .filter_map(|w| quota_values(w.observation_state, w.remaining_percent, w.used_percent).0)
            .min_by(f64::total_cmp)
    };
    let mut rows: Vec<_> = provider.accounts.iter().collect();
    rows.sort_by(|a, b| {
        let primary = b.is_active.cmp(&a.is_active).then_with(|| match sort {
            AccountSortOrder::SoonestReset => {
                next_reset(a).map_or((true, None), |v| (false, Some(v))).cmp(&next_reset(b).map_or((true, None), |v| (false, Some(v))))
            }
            AccountSortOrder::MostLeft => match (most_left(a), most_left(b)) {
                (Some(a), Some(b)) => b.total_cmp(&a),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                _ => Ordering::Equal,
            },
            AccountSortOrder::Name => Ordering::Equal,
        });
        primary.then_with(|| account_name(a, mask).to_lowercase().cmp(&account_name(b, mask).to_lowercase())).then_with(|| a.id.cmp(&b.id))
    });
    rows
}

#[component]
fn ProviderCard(provider: ProviderSummary, stale: bool) -> Element {
    let panel = use_context::<PanelState>();
    let prefs = (panel.settings)();
    let mut selected = use_signal(|| None::<String>);
    let id = &provider.descriptor.id;
    let selected_id = selected.cloned();
    let account = selected_id.as_ref().and_then(|id| provider.accounts.iter().find(|a| &a.id == id));
    let (health, tone) = provider_health(provider.descriptor.health, stale);
    let (mode, mode_tone) = crate::labels::adapter_mode(provider.descriptor.mode);
    let (verification, verification_tone) = crate::labels::verification(provider.descriptor.verification);
    let fixture_announced =
        provider.descriptor.mode == contracts::AdapterMode::Fixture
            && panel.status.read().data.as_ref().is_some_and(|s| {
                s.fixture_mode && !s.integrations.iter().any(|integration| integration.mode == contracts::AdapterMode::Live)
            });
    let has_quota = provider.descriptor.evidence_kinds.contains(&EvidenceKind::QuotaWindows)
        || !provider.pools.is_empty()
        || provider.accounts.iter().any(|a| !a.windows.is_empty());
    let count = provider.account_count;
    let account_label = if count == 1 { "account" } else { "accounts" };
    rsx! {
        section { class: "glass-card provider-card stack", style: "padding: 12px; gap: 8px;",
            div { class: "stack", style: "gap: 4px;",
                div { class: "row",
                    h2 { "{provider.descriptor.label}" }
                    if provider.descriptor.health == ProviderHealth::Unobserved {
                        span { class: "form-help", "{health}" }
                    } else {
                        Chip { label: health.to_string(), tone: Some(tone.to_string()) }
                    }
                }
                div { class: "row",
                    if !fixture_announced { Chip { label: mode.to_string(), tone: Some(mode_tone.to_string()) } }
                    if !fixture_announced || provider.descriptor.verification != contracts::Verification::FixtureTested {
                        Chip { label: verification.to_string(), tone: Some(verification_tone.to_string()) }
                    }
                    span { class: "form-help", "{count} {account_label} · {provider.enabled_accounts} enabled" }
                    if let Some(observed) = provider.descriptor.last_observed_at {
                        span { class: "form-help", "Observed " span { class: "nowrap-unit", "{fmt_dt(observed)}" } }
                    }
                }
                if let Some(message) = &provider.descriptor.status_message { p { "{message}" } }
            }
            div { class: "stack", style: "gap: 6px;",
                if has_quota || account.is_some() {
                    div { class: "row",
                        h3 { if let Some(a) = account { "{account_name(a, prefs.mask_emails)} · History & Forecast" } else { "Pool · History & Forecast" } }
                        if account.is_some() {
                            button { class: "btn ghost btn-xs", onclick: move |_| selected.set(None), "Back to Pool" }
                        }
                    }
                    {rsx! { QuotaPanel {
                        key: "{id}-{selected_id.as_deref().unwrap_or_default()}",
                        provider: provider.clone(), account: account.cloned(), lookback_default: prefs.drain_lookback_hours,
                        horizon_default: prefs.forecast_horizon_hours,
                    } }}
                }
                if provider.pools.is_empty() { p { class: "muted", if has_quota { "No pool readings yet." } else { "Quota windows not reported." } } }
                for pool in provider.pools.iter().filter(|p| !is_extra(p.kind)) {
                    PoolWindow { key: "{pool.window_key}", pool: pool.clone() }
                }
                if provider.pools.iter().any(|p| is_extra(p.kind)) {
                    details { summary { "Model and spend windows" }
                        for pool in provider.pools.iter().filter(|p| is_extra(p.kind)) {
                            PoolWindow { key: "{pool.window_key}", pool: pool.clone() }
                        }
                    }
                }
                OperationButton { provider: provider.clone(), label: "Collect pool quota", disabled: stale,
                    command: ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id: None }) }
                if !provider.accounts.is_empty() {
                    h3 { "Accounts" }
                    for account in sorted_accounts(&provider, prefs.account_sort, prefs.mask_emails) {
                        {
                            let account_id = account.id.clone();
                            rsx! { AccountSummary { key: "{account.id}", provider: provider.clone(), account: account.clone(), mask: prefs.mask_emails,
                                chart: move |_| selected.set(Some(account_id.clone())) } }
                        }
                    }
                }
                if supports(&provider, None, ProviderCapability::StartRollingWindow)
                    || provider.accounts.iter().any(|a| a.rolling_starter.is_some()) {
                    RollingStarter { provider: provider.clone() }
                }
            }
        }
    }
}

#[component]
fn PoolWindow(pool: PoolWindowSummary) -> Element {
    let weight_label = match pool.capacity_basis.as_str() {
        "equal_entitlements" => "Equal weights",
        "plan_tier_multiplier" | "plan_multiplier" => "Plan-tier weights",
        _ => "Provider-defined weights",
    };
    rsx! {
        div { class: "quota-window", style: "padding-block: 4px;",
            strong { "{window_label(&pool.label)}" }
            if let (Some(left), Some(used)) = quota_values(QuotaObservationState::Measured, pool.weighted_remaining_percent, pool.weighted_used_percent) {
                Meter { left, used }
            } else { span { class: "form-help", "Not observed" } }
            p { class: "muted",
                if pool.is_estimate { "Estimated pool · " }
                "{pool.included_accounts}/{pool.eligible_accounts} eligible accounts observed · "
                span { title: "{pool.capacity_basis}", "{weight_label}" }
                " · " ResetTime { at: pool.next_reset_at }
            }
        }
    }
}

#[component]
pub(super) fn AccountIdentity(
    provider: ProviderSummary,
    account: ModelAccount,
    mask: bool,
    #[props(default)] show_last_used: bool,
) -> Element {
    // Next pick is a routing hint, not an account health state.
    let (label, tone) = match account_status(&account, false) {
        AccountStatus::Disabled => ("Disabled", "muted"),
        AccountStatus::Limited => ("Limited", "warn"),
        AccountStatus::Error => ("Error", "bad"),
        _ if account.is_active => ("Active", "info"),
        _ => ("Ready", "ok"),
    };
    let credential = match account.enrollment_state {
        EnrollmentState::Selected if account.is_active => None,
        EnrollmentState::Error if label == "Error" => None,
        state => Some(enrollment_state_label(state)),
    };
    let email = if mask {
        account.masked_email.clone().or_else(|| account.email.as_deref().map(mask_email))
    } else {
        account.email.clone().or_else(|| account.masked_email.clone())
    };
    let name = account_name(&account, mask);
    let capacity = account.tier_multiplier.map(|tier| format!("{tier}×"));
    let message = account.status_message.clone().or_else(|| account.last_error.as_ref().map(AccountError::summary));
    let hint = account.last_error.as_ref().and_then(|error| error.action_hint.as_deref());
    rsx! {
        div { class: "row",
            strong { "{name}" }
            span { title: is_next_pick(&provider, &account).then_some("Next pick for routing"),
                Chip { label: label.to_string(), tone: Some(tone.to_string()) }
            }
            if let Some(credential) = credential {
                Chip { label: credential.to_string(), tone: Some("muted".to_string()) }
            }
        }
        div { class: "row muted",
            if let Some(email) = email.filter(|email| *email != name) { span { "{email}" } }
            if let Some(plan) = &account.plan { span { "{plan}" } }
            if let Some(capacity) = capacity.filter(|tier| !account.plan.as_deref().is_some_and(|plan| plan.split_whitespace().any(|part| part == tier))) {
                span { "{capacity} capacity" }
            }
            if account.is_preferred { span { class: "form-help", "Preferred for routing" } }
            if show_last_used { span { class: "form-help", "Last used: " span { class: "nowrap-unit", "{date(account.last_used_at)}" } } }
            if label == "Limited" { span { "Cooldown until " span { class: "nowrap-unit", "{date(account.cooldown_until)}" } } }
            if let Some(message) = &message { span { "{message}" } }
            if let Some(hint) = hint { span { "{hint}" } }
        }
    }
}

#[component]
fn AccountSummary(provider: ProviderSummary, account: ModelAccount, mask: bool, chart: EventHandler<()>) -> Element {
    rsx! {
        section { class: "provider-inset stack",
            div { class: "row",
                div { style: "flex: 1 1 12rem; min-width: 0;", AccountIdentity { provider: provider.clone(), account: account.clone(), mask } }
                div { class: "row",
                    if !account.windows.is_empty() || provider.descriptor.evidence_kinds.contains(&EvidenceKind::QuotaWindows) {
                        button { class: "btn btn-glass", onclick: move |_| chart.call(()), "Chart" }
                    }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Refresh quota",
                        command: ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id: Some(account.id.clone()) }) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Refresh reset credits",
                        command: ProviderCommand::Quota(QuotaCommand::RefreshGrants { account_id: Some(account.id.clone()) }) }
                }
            }
            if account.windows.is_empty() {
                p { class: "muted", "No quota windows · {fmt_count(account.success_count)} successes · {fmt_count(account.failure_count)} failures" }
            }
            for window in account.windows.iter().filter(|w| !is_extra(w.kind)) {
                QuotaLimitBar { key: "{window.window_key}", window: window.clone() }
            }
            if account.windows.iter().any(|w| is_extra(w.kind)) {
                details { summary { "Model and spend windows" }
                    for window in account.windows.iter().filter(|w| is_extra(w.kind)) {
                        QuotaLimitBar { key: "{window.window_key}", window: window.clone() }
                    }
                }
            }
            if let Some(grant) = &account.reset_grant {
                details { summary { "Reset credits / grants · {grant.available_count} available" }
                    p {
                        if grant.applicable { "Applicable" } else { "Not applicable" }
                        " · expires " span { class: "nowrap-unit", "{date(grant.expires_at)}" }
                        " · checked " span { class: "nowrap-unit", "{fmt_dt(grant.checked_at)}" }
                    }
                    p { {format!("Scope: {} · Clears: {} · Weekly credit-back: {}", grant.scope.as_deref().unwrap_or("—"), grant.cleared_windows.join(", "), grant.weekly_credit_back)} }
                    if let Some(reason) = &grant.unavailable_reason { p { "{reason}" } }
                    OperationButton { provider, account: account.clone(), label: "Claim Reset Credit", confirm: true,
                        disabled: !grant.applicable || grant.available_count == 0 || grant.expires_at.is_some_and(|t| t <= Utc::now()),
                        command: ProviderCommand::Quota(QuotaCommand::ClaimResetGrant { account_id: account.id.clone(), grant_id: grant.grant_id.clone() }) }
                }
            }
        }
    }
}

#[component]
fn QuotaPanel(provider: ProviderSummary, account: Option<ModelAccount>, lookback_default: u32, horizon_default: u32) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let lookback_default = if [3, 6, 12, 24].contains(&lookback_default) { lookback_default } else { 6 };
    let horizon_default = if [6, 12, 24, 48].contains(&horizon_default) { horizon_default } else { 12 };
    let mut lookback = use_signal(|| lookback_default);
    let mut horizon = use_signal(|| horizon_default);
    let mut range = use_signal(|| 24u32);
    let windows: BTreeMap<String, String> = if let Some(account) = &account {
        account.windows.iter().map(|w| (w.window_key.clone(), window_label(&w.label).to_string())).collect()
    } else {
        provider
            .pools
            .iter()
            .map(|w| (w.window_key.clone(), window_label(&w.label).to_string()))
            .chain(
                provider.accounts.iter().flat_map(|a| a.windows.iter().map(|w| (w.window_key.clone(), window_label(&w.label).to_string()))),
            )
            .collect()
    };
    let mut window = use_signal(String::new);
    let chosen = window.cloned();
    let chosen = windows.get_key_value(&chosen).or_else(|| windows.first_key_value()).map(|(key, _)| key.clone()).unwrap_or_default();
    let account_id = account.as_ref().map(|a| a.id.clone());
    let query = use_memo(use_reactive((&chosen, &account_id), move |(chosen, account_id)| QuotaQuery {
        account_id,
        window_key: optional(&chosen),
        range_hours: Some(range().max(lookback())),
        lookback_hours: Some(lookback()),
        horizon_hours: Some(horizon()),
    }));
    let id = segment(&provider.descriptor.id);
    let data = use_resource(move || {
        let _ = (panel.epoch)();
        let query = query();
        let (id, client) = (id.clone(), client.clone());
        async move {
            if query.window_key.is_none() {
                return Ok(None);
            }
            timed(client.provider_quota(&path_id(id)?, &query)).await.map(|data| Some((query, data)))
        }
    });
    let current_query = query();
    let result = data.read();
    let mask = panel.settings.read().mask_emails;
    rsx! {
        div { class: "form-grid-2", style: "grid-template-columns: repeat(auto-fit, minmax(min(100%, 5.75rem), 1fr)); gap: 4px 6px;",
            Choice { label: "Window", value: chosen.clone(), options: windows.into_iter().collect(), change: move |v| window.set(v) }
            Choice { label: "History", value: range().to_string(), options: pairs(&[("6", "6h"), ("12", "12h"), ("24", "24h"), ("48", "48h"), ("168", "7d")]), change: move |v: String| if let Ok(n) = v.parse() { range.set(n); } }
            Choice { label: "Drain lookback", value: lookback().to_string(), options: pairs(LOOKBACK), change: move |v: String| if let Ok(n) = v.parse() { lookback.set(n); } }
            Choice { label: "Forecast horizon", value: horizon().to_string(), options: pairs(HORIZON), change: move |v: String| if let Ok(n) = v.parse() { horizon.set(n); } }
        }
        {read_view(result.as_ref(), |data| {
            let Some((submitted, report)) = data else { return rsx! { p { class: "muted", "No quota window observed yet." } }; };
            if *submitted != current_query { return rsx! { Loading { text: "Loading quota range…".to_string() } }; }
            let forecasts = if account.is_some() {
                report.forecasts.iter().filter(|f| f.window_key == chosen && f.account_id == current_query.account_id).cloned().collect()
            } else {
                report.pools.iter().filter(|p| p.window_key == chosen).filter_map(|p| p.forecast.clone()).collect()
            };
            let names: BTreeMap<_, _> = report.accounts.iter().map(|a| (a.id.clone(), account_name(a, mask))).collect();
            let resets = report.samples.iter().filter(|s| s.observation_state == QuotaObservationState::Measured && !s.stale)
                .filter_map(|s| s.resets_at).chain(report.accounts.iter()
                    .filter(|a| current_query.account_id.as_ref().is_none_or(|id| id == &a.id))
                    .flat_map(|a| a.windows.iter()).filter(|w| w.window_key == chosen && !w.stale && w.observation_state == QuotaObservationState::Measured)
                    .filter_map(|w| w.resets_at)).collect();
            rsx! {
                QuotaHistoryChart { samples: report.samples.clone(), forecasts, resets, names: names.clone(),
                    from: report.as_of - TimeDelta::hours(range().into()), until: report.as_of + TimeDelta::hours(horizon().into()) }
                details { summary { "Past Windows" }
                    if account.is_none() { p { class: "form-help", "Pool history plots each account separately; incompatible quota windows aren’t averaged." } }
                    Table { label: "Past windows", headers: &["Account", "Window", "Started", "Ended", "Peak used %", "Exhausted", "Early replacement", "Samples"],
                        rows: report.past_windows.iter().rev().map(|w| vec![
                            names.get(&w.account_id).cloned().unwrap_or_else(|| w.account_id.clone()), window_label(&w.label).to_string(), fmt_dt(w.started_at),
                            fmt_dt(w.ended_at), number(w.peak_used_percent), date(w.exhausted_at), if w.early_replaced { "Yes" } else { "No" }.into(), w.sample_count.to_string()
                        ]).collect() }
                }
            }
        })}
    }
}

#[component]
fn RollingStarter(provider: ProviderSummary) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let available = supports(&provider, None, ProviderCapability::StartRollingWindow);
    let id = segment(&provider.descriptor.id);
    let data = use_resource(move || {
        let client = client.clone();
        let id = id.clone();
        let _ = (panel.epoch)();
        async move { if available { timed(client.provider_routing(&path_id(id)?)).await.map(Some) } else { Ok(None) } }
    });
    let current = data.read();
    let mask = panel.settings.read().mask_emails;
    rsx! {
        details { class: "provider-inset", summary { "Rolling Window Starter" }
            p { class: "muted", "Opt-in starter sends real requests and consumes quota." }
            for account in provider.accounts.iter().filter(|a| a.rolling_starter.is_some()) {
                if let Some(s) = &account.rolling_starter {
                    p { key: "{account.id}", "{account_name(account, mask)} · " if s.enabled { "Enabled" } else { "Off" }
                        " · last start " span { class: "nowrap-unit", "{date(s.last_started_at)}" }
                        " · next " span { class: "nowrap-unit", "{date(s.next_scheduled_start_at)}" }
                        if let Some(error) = &s.last_error { " · {error}" }
                    }
                }
            }
            {read_view(current.as_ref(), |data| match data {
                Some(view) => rsx! { RollingEditor { key: "{view.revision}-{panel.epoch}", provider, initial: view.rolling_starter.clone(), revision: view.revision } },
                None => rsx! { p { class: "muted", "Starter controls unavailable." } },
            })}
        }
    }
}

#[component]
fn RollingEditor(provider: ProviderSummary, initial: RollingWindowStarterConfig, revision: i64) -> Element {
    let mut enabled = use_signal(|| initial.enabled);
    let mut model = use_signal(|| initial.model_id.clone().unwrap_or_default());
    let mut stagger = use_signal(|| initial.stagger_mins.to_string());
    let mut start = use_signal(|| initial.active_hours_start.map(|v| v.to_string()).unwrap_or_default());
    let mut end = use_signal(|| initial.active_hours_end.map(|v| v.to_string()).unwrap_or_default());
    let parse_hour = |s: &str| if s.is_empty() { Ok(None) } else { s.parse::<u8>().ok().filter(|n| *n <= 23).map(Some).ok_or(()) };
    let (start_hour, end_hour, stagger_mins) = (parse_hour(&start()), parse_hour(&end()), stagger().parse::<u32>());
    let valid = matches!((&start_hour, &end_hour), (Ok(None), Ok(None)) | (Ok(Some(_)), Ok(Some(_)))) && stagger_mins.is_ok();
    let config = RollingWindowStarterConfig {
        enabled: enabled(),
        provider_id: Some(provider.descriptor.id.clone()),
        model_id: optional(&model()),
        stagger_mins: stagger_mins.unwrap_or_default(),
        active_hours_start: start_hour.ok().flatten(),
        active_hours_end: end_hour.ok().flatten(),
    };
    let mask = use_context::<PanelState>().settings.read().mask_emails;
    rsx! {
        div { class: "row",
            Check { label: "Opt in", value: enabled(), change: move |v| enabled.set(v) }
            Field { label: "Model ID", value: model(), change: move |v| model.set(v) }
            Field { label: "Stagger minutes", kind: "number", value: stagger(), change: move |v| stagger.set(v) }
            Field { label: "Active from hour (0–23)", kind: "number", value: start(), change: move |v| start.set(v) }
            Field { label: "Active until hour (0–23)", kind: "number", value: end(), change: move |v| end.set(v) }
            OperationButton { provider: provider.clone(), revision, label: "Save starter", disabled: !valid,
                command: ProviderCommand::Quota(QuotaCommand::ConfigureRollingStarter { config }) }
        }
        for account in &provider.accounts {
            if supports(&provider, Some(account), ProviderCapability::StartRollingWindow) {
                div { key: "{account.id}", class: "row",
                    span { "{account_name(account, mask)}" }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Start Window", disabled: !account.enabled, confirm: true,
                        command: ProviderCommand::Quota(QuotaCommand::TriggerRollingStart { account_id: account.id.clone(), model_id: optional(&model()) }) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_summary_uses_the_lowest_observed_regular_window() {
        let session = PoolWindowSummary {
            provider_id: "test".into(),
            window_key: "session".into(),
            label: "Session".into(),
            kind: QuotaWindowKind::Session,
            eligible_accounts: 1,
            included_accounts: 1,
            weighted_used_percent: Some(65.0),
            weighted_remaining_percent: None,
            capacity_basis: "equal_entitlements".into(),
            next_reset_at: None,
            forecast: None,
            is_estimate: true,
        };
        let weekly = PoolWindowSummary { kind: QuotaWindowKind::Weekly, weighted_remaining_percent: Some(70.0), ..session.clone() };
        let spend = PoolWindowSummary { kind: QuotaWindowKind::Spend, weighted_remaining_percent: Some(0.0), ..session.clone() };
        assert_eq!(pool_quota_summary(&[weekly, spend, session]), "≈35.0% left (Session)");
    }
}
