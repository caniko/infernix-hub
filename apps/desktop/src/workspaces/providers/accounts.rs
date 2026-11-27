use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
};

use chrono::TimeDelta;

use super::{
    charts::*,
    dashboard::{HORIZON, LOOKBACK, NEXT_PICK_HINT, account_status, is_extra, is_next_pick, meter_forecast},
    *,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum AccountListSort {
    #[default]
    Capacity,
    Usage,
    MostLeft,
    NextReset,
    Name,
}

impl AccountListSort {
    const OPTIONS: &[(&str, &str)] =
        &[("capacity", "Capacity"), ("usage", "Most Used"), ("most-left", "Most Left"), ("reset", "Next Reset"), ("name", "Name")];

    fn from_setting(order: AccountSortOrder) -> Self {
        match order {
            AccountSortOrder::SoonestReset => Self::NextReset,
            AccountSortOrder::MostLeft => Self::MostLeft,
            AccountSortOrder::Name => Self::Name,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Capacity => "capacity",
            Self::Usage => "usage",
            Self::MostLeft => "most-left",
            Self::NextReset => "reset",
            Self::Name => "name",
        }
    }
}

fn regular_windows(account: &ModelAccount) -> impl Iterator<Item = &QuotaWindow> {
    account.windows.iter().filter(|window| !is_extra(window.kind))
}

fn tightest_remaining(account: &ModelAccount) -> Option<f64> {
    regular_windows(account).filter_map(|w| quota_values(w.observation_state, w.remaining_percent, w.used_percent).0).min_by(f64::total_cmp)
}

fn account_name(account: &ModelAccount, mask: bool) -> String {
    if !account.display_name.trim().is_empty() {
        dashboard::account_name(account, mask)
    } else if mask && account.id.contains('@') {
        mask_email(&account.id)
    } else {
        account.id.clone()
    }
}

fn reports_next_pick(provider: &ProviderSummary, account: &ModelAccount) -> bool {
    account.is_next_pick || provider.next_pick_account_id.as_ref() == Some(&account.id)
}

fn sort_account_rows_at(rows: &mut Vec<(&ProviderSummary, &ModelAccount)>, sort: AccountListSort, now: DateTime<Utc>) {
    rows.sort_by(|(ap, a), (bp, b)| {
        let tier = |account: &ModelAccount| account.tier_multiplier.filter(|v| v.is_finite() && *v > 0.0).unwrap_or(1.0);
        let reset = |account: &ModelAccount| regular_windows(account).filter_map(|w| w.resets_at).filter(|at| *at > now).min();
        let primary = match sort {
            AccountListSort::Capacity => tier(b)
                .total_cmp(&tier(a))
                .then_with(|| b.is_active.cmp(&a.is_active))
                .then_with(|| reports_next_pick(bp, b).cmp(&reports_next_pick(ap, a)))
                .then_with(|| b.priority.cmp(&a.priority)),
            AccountListSort::Usage | AccountListSort::MostLeft => match (tightest_remaining(a), tightest_remaining(b)) {
                (Some(a), Some(b)) if sort == AccountListSort::MostLeft => b.total_cmp(&a),
                (Some(a), Some(b)) => a.total_cmp(&b),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                _ => Ordering::Equal,
            },
            AccountListSort::NextReset => {
                let (a, b) = (reset(a), reset(b));
                (a.is_none(), a).cmp(&(b.is_none(), b))
            }
            AccountListSort::Name => Ordering::Equal,
        };
        primary
            .then_with(|| account_name(a, false).to_lowercase().cmp(&account_name(b, false).to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| ap.descriptor.id.cmp(&bp.descriptor.id))
    });
}

fn pool_share_percent(tier: Option<f64>, total: Option<f64>) -> Option<f64> {
    let tier = tier.filter(|n| n.is_finite() && *n > 0.0)?;
    let total = total.filter(|n| n.is_finite() && *n > 0.0)?;
    let percent = (tier / total) * 100.0;
    percent.is_finite().then_some(percent)
}

fn tier_label(account: &ModelAccount) -> Option<String> {
    let tier = account.tier_multiplier.map(format_multiplier);
    match (tier, account.plan.as_deref().and_then(optional)) {
        (Some(tier), Some(plan)) if !plan.split_whitespace().any(|part| part == tier) => Some(format!("{tier} {plan}")),
        (_, Some(plan)) => Some(plan),
        (tier, None) => tier,
    }
}

fn account_state(provider: &ProviderSummary, account: &ModelAccount) -> (&'static str, &'static str) {
    let status = account_status(account, reports_next_pick(provider, account));
    match status {
        AccountStatus::Disabled => ("Disabled", "muted"),
        AccountStatus::Limited => ("Limited", "warn"),
        AccountStatus::Error => ("Error", "bad"),
        _ if account.is_active => ("Active", "info"),
        AccountStatus::NextPick => ("Next pick", "info"),
        _ => ("Ready", "ok"),
    }
}

fn rail_reset(account: &ModelAccount, now: DateTime<Utc>) -> Option<String> {
    let reset = regular_windows(account).filter_map(|w| w.resets_at).filter(|at| *at > now).min()?;
    let mins = (reset - now).num_minutes();
    Some(match mins {
        0 => "Reset <1m".into(),
        1..=59 => format!("Reset {mins}m"),
        60..=1439 => format!("Reset {}h {:02}m", mins / 60, mins % 60),
        _ => format!("Reset {}d {}h", mins / 1440, mins / 60 % 24),
    })
}

#[component]
pub(super) fn Accounts(providers: Vec<ProviderSummary>) -> Element {
    let mut panel = use_context::<PanelState>();
    let client = use_context::<HubClient>();
    let mut key_groups = use_signal(|| false);
    let mut connecting = use_signal(|| false);
    let mut sort_override = use_signal(|| None);
    let mut selected_key = use_signal(|| None::<(String, String)>);
    let show_history = use_signal(|| false);
    let settings = panel.settings.cloned();
    let sort = sort_override().unwrap_or_else(|| AccountListSort::from_setting(settings.account_sort));
    let selection = panel.selected_provider.cloned();
    let ready = (panel.settings_ready)();
    let operator = panel.operator();
    let now = Utc::now();
    let scoped_providers: Vec<&ProviderSummary> =
        providers.iter().filter(|p| selection.is_empty() || p.descriptor.id == selection).collect();
    let mut rows: Vec<_> = scoped_providers.iter().copied().flat_map(|p| p.accounts.iter().map(move |a| (p, a))).collect();
    sort_account_rows_at(&mut rows, sort, now);
    let current = selected_key.cloned();
    let selected = rows
        .iter()
        .copied()
        .find(|(p, a)| current.as_ref().is_some_and(|(provider, account)| *provider == p.descriptor.id && *account == a.id))
        .or_else(|| rows.first().copied());
    let effective_key = selected.map(|(p, a)| (p.descriptor.id.clone(), a.id.clone()));
    use_effect(use_reactive((&effective_key,), move |(key,)| {
        if *selected_key.peek() != key {
            selected_key.set(key);
        }
    }));
    let can_add = |p: &ProviderSummary| {
        operator && [ProviderCapability::BeginOAuth, ProviderCapability::ImportLogin].into_iter().any(|cap| supports(p, None, cap))
    };
    rsx! {
        nav { class: "accounts-subtabs tab-bar", aria_label: "Account workspace views",
            button { class: if !key_groups() { "tab-btn active" } else { "tab-btn" },
                aria_current: if !key_groups() { "page" } else { "false" }, onclick: move |_| key_groups.set(false), "Accounts" }
            button { class: if key_groups() { "tab-btn active" } else { "tab-btn" },
                aria_current: if key_groups() { "page" } else { "false" }, onclick: move |_| key_groups.set(true), "Key Groups & Models" }
        }
        div { class: "accounts-toolbar row",
            div { class: "row", role: "group", aria_label: "Account provider scope",
                button { class: "filter-chip", aria_pressed: selection.is_empty(), onclick: move |_| panel.selected_provider.set(String::new()), "All" }
                for provider in &providers {
                    {
                        let id = provider.descriptor.id.clone();
                        rsx! { button { key: "{id}", class: "filter-chip", aria_pressed: selection == id,
                            onclick: move |_| panel.selected_provider.set(id.clone()),
                            "{provider.descriptor.label} ({provider.accounts.len()})" } }
                    }
                }
            }
            if !key_groups() {
                div { class: "row accounts-tools",
                    Choice { label: "Sort accounts", value: sort.key(), options: pairs(AccountListSort::OPTIONS), change: move |value: String| {
                        sort_override.set(Some(match value.as_str() {
                            "usage" => AccountListSort::Usage,
                            "most-left" => AccountListSort::MostLeft,
                            "reset" => AccountListSort::NextReset,
                            "name" => AccountListSort::Name,
                            _ => AccountListSort::Capacity,
                        }));
                    } }
                    Check { label: "Mask emails", value: settings.mask_emails, disabled: !ready, change: move |value| {
                        panel.settings.write().mask_emails = value;
                        if panel.operator() && ready { schedule_toolbar_settings_save(panel, client.clone()); }
                    } }
                    if scoped_providers.iter().copied().any(can_add) {
                        button { class: "btn primary", aria_expanded: connecting(), aria_controls: "accounts-connect",
                            onclick: move |_| connecting.toggle(), "+ Connect / Import" }
                    }
                }
            }
        }
        if key_groups() {
            for provider in scoped_providers.iter().copied() {
                section { key: "{provider.descriptor.id}", class: "glass-card stack",
                    h2 { "{provider.descriptor.label} · Key Groups & Models" }
                    KeyGroups { provider: provider.clone() }
                }
            }
        } else {
            div { id: "accounts-connect", class: "stack", hidden: !connecting(),
                if connecting() {
                    for provider in scoped_providers.iter().copied().filter(|p| can_add(p)) {
                        AccountConnect { key: "{provider.descriptor.id}", provider: provider.clone() }
                    }
                }
            }
            if operator && scoped_providers.iter().copied().any(|p| supports(p, None, ProviderCapability::RenewLogin)) {
                div { class: "row accounts-renew",
                    for provider in scoped_providers.iter().copied().filter(|p| supports(p, None, ProviderCapability::RenewLogin)) {
                        div { class: "row", key: "{provider.descriptor.id}",
                            if scoped_providers.len() > 1 { span { class: "form-help", "{provider.descriptor.label}" } }
                            OperationButton { provider: provider.clone(), label: "Renew all logins", confirm: true,
                                command: ProviderCommand::Credential(CredentialCommand::RenewLogin { account_id: None }) }
                        }
                    }
                }
            }
            if let Some((provider, account)) = selected {
                div { class: "accounts-split",
                    aside { class: "accounts-rail", aria_label: "Provider accounts",
                        div { class: "account-rail-heading", "{rows.len()} accounts" }
                        for (p, a) in rows {
                            {
                                let key = (p.descriptor.id.clone(), a.id.clone());
                                let active = effective_key.as_ref() == Some(&key);
                                let name = account_name(a, settings.mask_emails);
                                let tier = tier_label(a);
                                let (status, tone) = account_state(p, a);
                                let reset = rail_reset(a, now);
                                let remaining = tightest_remaining(a);
                                let meter_tone = match remaining { Some(p) if p < 10.0 => "bad", Some(p) if p < 50.0 => "warn", _ => "ok" };
                                rsx! {
                                    button { key: "{key:?}", class: if active { "account-rail-item active" } else { "account-rail-item" },
                                        aria_pressed: active, title: "{p.descriptor.label} · {name} · {status}", onclick: move |_| selected_key.set(Some(key.clone())),
                                        span { class: "account-rail-top",
                                            span { class: "status-dot {tone}", aria_hidden: "true" }
                                            span { class: "account-rail-name", "{name}" }
                                            if let Some(tier) = tier { span { class: "tier-badge", "{tier}" } }
                                        }
                                        span { class: "account-rail-bot",
                                            if let Some(left) = remaining {
                                                span { class: "account-rail-meter", aria_hidden: "true",
                                                    span { class: "{meter_tone}", style: "width: {left.clamp(0.0, 100.0)}%" } }
                                                span { "{left:.0}% left" }
                                            }
                                            span { "{status}" }
                                            if let Some(reset) = reset { span { "{reset}" } }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    AccountInspector { key: "{provider.descriptor.id}:{account.id}", provider: provider.clone(), account: account.clone(),
                        mask: settings.mask_emails, show_history }
                }
            } else {
                p { class: "muted", "No logins or account bindings observed." }
            }
        }
    }
}

#[component]
fn AccountConnect(provider: ProviderSummary) -> Element {
    let mut hint = use_signal(String::new);
    let mut login_label = use_signal(String::new);
    let mut import_label = use_signal(String::new);
    let operator = use_context::<PanelState>().operator();
    let can_import = operator && supports(&provider, None, ProviderCapability::ImportLogin);
    let can_connect = operator && supports(&provider, None, ProviderCapability::BeginOAuth);
    rsx! {
        section { class: "accounts-connect provider-inset stack",
            h2 { "{provider.descriptor.label} · Logins" }
            if can_connect {
                div { class: "row", role: "group", aria_label: "Add an account",
                    Field { label: "New login label (optional)", value: login_label(), change: move |v| login_label.set(v) }
                    OperationButton { provider: provider.clone(), label: "Connect account", tone: "primary",
                        command: ProviderCommand::Credential(CredentialCommand::BeginOAuth {
                            account_label: optional(&login_label()), host: None, unix_user: None,
                        }) }
                }
            }
            if can_import {
                details { summary { "Import an existing login" }
                    p { class: "form-help", "Use the source reference understood by the provider’s credential owner. Do not paste tokens or credential JSON." }
                    div { class: "row",
                        Field { label: "Login source reference", value: hint(), change: move |v| hint.set(v) }
                        Field { label: "Imported login label (optional)", value: import_label(), change: move |v| import_label.set(v) }
                        OperationButton { provider: provider.clone(), label: "Import login", confirm: true, disabled: optional(&hint()).is_none(),
                            command: ProviderCommand::Credential(CredentialCommand::ImportLogin { label: optional(&import_label()), source_hint: hint().trim().into() }) }
                    }
                }
            }
        }
    }
}

#[component]
fn AccountInspector(provider: ProviderSummary, account: ModelAccount, mask: bool, mut show_history: Signal<bool>) -> Element {
    use ProviderCapability::*;
    let panel = use_context::<PanelState>();
    let mut editing = use_signal(|| false);
    let now = Utc::now();
    let (status, tone) = account_state(&provider, &account);
    let name = account_name(&account, mask);
    let email = if mask {
        account.masked_email.clone().or_else(|| account.email.as_deref().map(mask_email))
    } else {
        account.email.clone().or_else(|| account.masked_email.clone())
    };
    let total = provider_total_capacity(&provider);
    let retained_unrouted = provider.routed_account_ids.as_ref().is_some_and(|ids| !ids.contains(&account.id));
    let share = pool_share_percent(account.tier_multiplier, total)
        .zip(total)
        .filter(|_| account.enabled && !retained_unrouted)
        .map(|(share, total)| format!("{share:.0}% of {} pool", format_multiplier(total)));
    let tier = tier_label(&account);
    let can_edit = panel.operator()
        && [SetCapacity, SetPriority, SetWeight, SetNote, SetEnabled].into_iter().any(|cap| supports(&provider, Some(&account), cap));
    let hint = account.last_error.as_ref().and_then(|error| error.action_hint.as_deref());
    let error = account.last_error.as_ref().map(AccountError::summary);
    let message = account.status_message.as_ref();
    let advisory = capacity_ratio_mismatch(account.tier_multiplier, account.observed_capacity_ratio);
    let prefs = panel.settings.cloned();
    rsx! {
        article { class: "account-inspector stack", aria_label: "Account details",
            header { class: "account-inspector-head row",
                div { class: "stack",
                    div { class: "row",
                        h2 { "{name}" }
                        Chip { label: status.to_string(), tone: Some(tone.to_string()) }
                        Chip { label: enrollment_state_label(account.enrollment_state).to_string(), tone: Some("muted".into()) }
                        if retained_unrouted {
                            Chip { label: "Other account quota history".to_string(), tone: Some("muted".into()) }
                        }
                    }
                    div { class: "row muted",
                        span { "{provider.descriptor.label}" }
                        if let Some(email) = email.filter(|email| email != &name) { span { "{email}" } }
                        if let Some(tier) = tier { span { class: "tier-badge", "{tier}" } }
                        if let Some(share) = &share { span { "{share}" } }
                    }
                }
                div { class: "row",
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Refresh quota",
                        command: ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id: Some(account.id.clone()) }) }
                    if can_edit {
                        button { class: "btn secondary", aria_expanded: editing(), aria_controls: "inspector-capacity",
                            onclick: move |_| editing.toggle(), if editing() { "Done editing" } else { "Edit" } }
                    }
                }
                div { class: "account-inspector-notices form-help",
                    if account.is_preferred { p { "Preferred for routing" } }
                    if is_next_pick(&provider, &account) { p { "{NEXT_PICK_HINT}" } }
                    if let Some(until) = account.cooldown_until.filter(|at| *at > now) { p { "Cooldown until {fmt_dt(until)}" } }
                    if let Some(message) = message { p { "{message}" } }
                    if let Some(error) = error.filter(|detail| Some(detail) != message) { p { "{error}" } }
                    if let Some(hint) = hint { p { "{hint}" } }
                }
            }
            section { class: "inspector-section stack quota-meter-group",
                div { class: "row quota-meters-header",
                    h3 { "Quota Windows" }
                    QuotaMeterKey {}
                    button { class: "btn ghost", aria_expanded: show_history(), aria_controls: "account-quota-history",
                        onclick: move |_| show_history.toggle(),
                        if show_history() { "Hide quota history" } else { "Show account quota history" } }
                }
                AccountQuotaWindows { provider: provider.clone(), account: account.clone() }
                if account.reset_grant.is_some() || supports(&provider, Some(&account), RefreshGrants) {
                    div { class: "row",
                        h4 { "Reset credits" }
                        OperationButton { provider: provider.clone(), account: account.clone(), label: "Refresh reset credits",
                            command: ProviderCommand::Quota(QuotaCommand::RefreshGrants { account_id: Some(account.id.clone()) }) }
                    }
                    if let Some(grant) = &account.reset_grant {
                        details {
                            summary { "{grant.available_count} available" }
                            p {
                                if grant.applicable { "Applicable" } else { "Not applicable" }
                                " · expires {date(grant.expires_at)} · checked {fmt_dt(grant.checked_at)}"
                            }
                            p { "Scope: {grant.scope.as_deref().unwrap_or(\"—\")} · Clears: {grant.cleared_windows.join(\", \")} · Weekly credit-back: {grant.weekly_credit_back}" }
                            if let Some(reason) = &grant.unavailable_reason { p { "{reason}" } }
                            OperationButton { provider: provider.clone(), account: account.clone(), label: "Claim Reset Credit", confirm: true,
                                disabled: !grant.applicable || grant.available_count == 0 || grant.expires_at.is_some_and(|at| at <= now),
                                command: ProviderCommand::Quota(QuotaCommand::ClaimResetGrant {
                                    account_id: account.id.clone(), grant_id: grant.grant_id.clone(),
                                }) }
                        }
                    } else {
                        p { class: "form-help", "No reset credit observation." }
                    }
                }
                if account.rolling_starter.is_some() || supports(&provider, Some(&account), StartRollingWindow) {
                    AccountRollingStarter { provider: provider.clone(), account: account.clone() }
                }
                div { id: "account-quota-history", class: "stack",
                    if show_history() {
                        p { class: "form-help", "Other account quota history: select another account in the list to switch." }
                        AccountQuotaHistory { provider_id: provider.descriptor.id.clone(), account: account.clone(),
                            lookback_default: prefs.drain_lookback_hours, horizon_default: prefs.forecast_horizon_hours }
                    }
                }
            }
            section { id: "inspector-capacity", class: "inspector-section stack",
                h3 { "Capacity & Routing" }
                if editing() && can_edit {
                    AccountCapacityEditor { provider: provider.clone(), account: account.clone(), done: move |_| editing.set(false) }
                } else {
                    dl { class: "inspector-kv-grid",
                        div { dt { "Plan & tier" } dd {
                            "{account.plan.as_deref().unwrap_or(\"—\")} · "
                            {account.tier_multiplier.map(format_multiplier).unwrap_or_else(|| "—".into())}
                            if let Some(share) = &share { " · {share}" }
                        } }
                        if let Some(advisory) = advisory {
                            div { dt { "Observed capacity (advisory)" } dd {
                                span { class: if advisory.mismatch { "capacity-advisory-badge mismatch" } else { "capacity-advisory-badge" },
                                    title: "{advisory.detail}", "{advisory.observed_label}" }
                                small { "{advisory.detail}" }
                            } }
                        }
                        div { dt { "Routing priority & weight" } dd {
                            "Priority {account.priority} · Weight {account.weight} · "
                            if account.enabled { "Enabled" } else { "Disabled" }
                        } }
                        div { dt { "Lifetime requests" } dd { "{fmt_count(account.success_count)} succeeded · {fmt_count(account.failure_count)} failed" } }
                        div { dt { "Operator note" } dd { "{account.note.as_deref().unwrap_or(\"—\")}" } }
                    }
                }
            }
            AccountAutomation { provider, account, mask }
        }
    }
}

#[component]
fn AccountCapacityEditor(provider: ProviderSummary, account: ModelAccount, done: EventHandler<()>) -> Element {
    let mut plan = use_signal(|| account.plan.clone().unwrap_or_default());
    let mut multiplier = use_signal(|| account.tier_multiplier.map(|tier| tier.to_string()).unwrap_or_default());
    let multiplier_input = multiplier();
    let tier_multiplier = multiplier_input.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0);
    let mut priority = use_signal(|| account.priority.to_string());
    let mut weight = use_signal(|| account.weight.to_string());
    let mut note = use_signal(|| account.note.clone().unwrap_or_default());
    let id = &account.id;
    let can = |cap| supports(&provider, Some(&account), cap);
    rsx! {
        div { class: "inspector-edit-grid",
            if can(ProviderCapability::SetCapacity) {
                div { class: "row",
                    Field { label: "Plan", value: plan(), change: move |v| plan.set(v) }
                    label { class: "provider-field", span { "Tier multiplier" }
                        input { class: "input", r#type: "number", step: "any", value: multiplier_input,
                            oninput: move |e| multiplier.set(e.value()) }
                    }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Save capacity",
                        disabled: !multiplier().trim().is_empty() && tier_multiplier.is_none(),
                        command: ProviderCommand::Account(AccountCommand::SetCapacity { account_id: id.clone(), plan: optional(&plan()), tier_multiplier }) }
                }
            }
            if can(ProviderCapability::SetPriority) {
                div { class: "row",
                    Field { label: "Priority", kind: "number", value: priority(), change: move |v| priority.set(v) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Save priority", disabled: priority().parse::<i32>().is_err(),
                        command: ProviderCommand::Account(AccountCommand::SetPriority { account_id: id.clone(), priority: priority().parse().unwrap_or_default() }) }
                }
            }
            if can(ProviderCapability::SetWeight) {
                div { class: "row",
                    Field { label: "Weight", kind: "number", value: weight(), change: move |v| weight.set(v) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Save weight", disabled: !weight().parse::<u32>().is_ok_and(|n| n > 0),
                        command: ProviderCommand::Account(AccountCommand::SetWeight { account_id: id.clone(), weight: weight().parse().unwrap_or_default() }) }
                }
            }
            if can(ProviderCapability::SetNote) {
                div { class: "row",
                    Field { label: "Note", value: note(), change: move |v| note.set(v) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Save note",
                        command: ProviderCommand::Account(AccountCommand::SetNote { account_id: id.clone(), note: optional(&note()) }) }
                }
            }
            div { class: "row",
                button { class: "btn secondary", onclick: move |_| done.call(()), "Done editing" }
            }
        }
    }
}

#[component]
fn AccountEnabled(provider: ProviderSummary, account: ModelAccount) -> Element {
    let label = if account.enabled { "Disable" } else { "Enable" };
    rsx! { OperationButton { provider, account: account.clone(), label, confirm: account.enabled,
    command: ProviderCommand::Account(AccountCommand::SetEnabled { account_id: account.id, enabled: !account.enabled }) } }
}

#[component]
fn AccountAutomation(provider: ProviderSummary, account: ModelAccount, mask: bool) -> Element {
    let id = account.id.clone();
    let mut scope = use_signal(|| "host".to_string());
    let mut target = use_signal(|| account.host.clone().unwrap_or_default());
    let source = &account.source_account_id;
    let source = if mask && source.contains('@') { mask_email(source) } else { source.clone() };
    let now = Utc::now();
    let timestamp =
        |at: Option<DateTime<Utc>>| at.map_or_else(|| "—".into(), |at| format!("{} · {}", fmt_dt(at), format_relative_age(at, now)));
    let operator = use_context::<PanelState>().operator();
    let can_edit = |cap| operator && supports(&provider, Some(&account), cap);
    let can_switch = !account.is_active && can_edit(ProviderCapability::SelectLocalAccount);
    let edit_persistent = can_edit(ProviderCapability::SelectPersistentAccount);
    rsx! {
        section { class: "inspector-section stack",
            h3 { "Automation & Auth" }
            dl { class: "inspector-kv-grid",
                div { dt { "Source account ID" } dd { "{source}" } }
                div { dt { "Host" } dd { "{account.host.as_deref().unwrap_or(\"—\")}" } }
                div { dt { "Unix user" } dd { "{account.unix_user.as_deref().unwrap_or(\"—\")}" } }
                div { dt { "Credential fingerprint" } dd { "{account.credential_fingerprint.as_deref().unwrap_or(\"—\")}" } }
                div { dt { "Last used" } dd { {timestamp(account.last_used_at)} } }
                div { dt { "Last observed" } dd { {timestamp(account.last_observed_at)} } }
            }
            if can_switch {
                div { class: "row",
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Switch local account", disabled: !account.enabled,
                        command: ProviderCommand::Account(AccountCommand::SelectLocalAccount {
                            account_id: id.clone(), host: account.host.clone(), unix_user: account.unix_user.clone(),
                        }) }
                }
            }
            if edit_persistent {
                div { class: "row",
                    Choice { label: "Persistent selection scope", value: scope(), options: pairs(&[("host", "Host"), ("user", "User"), ("fleet", "Fleet")]),
                        change: move |v: String| { target.set(String::new()); scope.set(v); } }
                    Field { label: "Persistent target (optional)", value: target(), change: move |v| target.set(v) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Select persistent account", confirm: true,
                        disabled: !account.enabled,
                        command: ProviderCommand::Account(AccountCommand::SelectPersistentAccount { account_id: id.clone(), scope: scope(), target: optional(&target()) }) }
                }
            }
            div { class: "inspector-danger-row row",
                div { class: "row",
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Renew login",
                        command: ProviderCommand::Credential(CredentialCommand::RenewLogin { account_id: Some(id.clone()) }) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Export login", confirm: true,
                        command: ProviderCommand::Credential(CredentialCommand::ExportLogin { account_id: id.clone() }) }
                    AccountEnabled { provider: provider.clone(), account: account.clone() }
                }
                OperationButton { provider, account: account.clone(), label: "Remove login", tone: "danger", confirm: true,
                    command: ProviderCommand::Credential(CredentialCommand::RemoveLogin { account_id: id }) }
            }
        }
    }
}

#[component]
fn AccountQuotaWindows(provider: ProviderSummary, account: ModelAccount) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let provider_id = segment(&provider.descriptor.id);
    let account_id = account.id.clone();
    let has_meter = account.windows.iter().any(|w| w.resets_at.is_some());
    let data = use_resource(use_reactive((&provider_id, &account_id, &has_meter), move |(provider_id, account_id, has_meter)| {
        let _ = (panel.epoch)();
        let lookback = panel.settings.read().drain_lookback_hours;
        let client = client.clone();
        async move {
            if !has_meter {
                return None;
            }
            let query = QuotaQuery {
                account_id: Some(account_id),
                range_hours: Some(1),
                lookback_hours: Some(lookback),
                horizon_hours: Some(1),
                ..Default::default()
            };
            let report = timed(client.provider_quota(&path_id(provider_id).ok()?, &query)).await.ok()?;
            Some(report.forecasts)
        }
    }));
    let forecasts = data.read();
    let forecast = |window: &QuotaWindow| {
        meter_forecast(forecasts.as_ref()?.as_deref()?, Some(&account.id), &window.window_key, window.scope_model.as_deref())
    };
    rsx! {
        if account.windows.is_empty() { p { class: "muted", "No quota reported for this account." } }
        for window in regular_windows(&account) {
            QuotaLimitBar { key: "{window.window_key}:{window.scope_model:?}", window: window.clone(), forecast: forecast(window) }
        }
        if account.windows.iter().any(|w| is_extra(w.kind)) {
            details { summary { "Model and spend windows" }
                for window in account.windows.iter().filter(|w| is_extra(w.kind)) {
                    QuotaLimitBar { key: "{window.window_key}:{window.scope_model:?}", window: window.clone(), forecast: forecast(window) }
                }
            }
        }
    }
}

#[component]
fn AccountRollingStarter(provider: ProviderSummary, account: ModelAccount) -> Element {
    let panel = use_context::<PanelState>();
    let client = use_context::<HubClient>();
    let id = segment(&provider.descriptor.id);
    let can_start = panel.operator() && supports(&provider, Some(&account), ProviderCapability::StartRollingWindow);
    let needs_model = provider.descriptor.router_id.is_some() && provider.descriptor.mode == AdapterMode::Live;
    let mut model = use_signal(|| None::<String>);
    let config = use_resource(use_reactive((&can_start, &id), move |(can_start, id)| {
        let _ = (panel.epoch)();
        let client = client.clone();
        async move {
            if !can_start {
                return None;
            }
            timed(client.provider_routing(&path_id(id).ok()?)).await.ok()?.rolling_starter.model_id
        }
    }));
    let configured = config.read().as_ref().and_then(Clone::clone);
    let draft = model().or(configured).unwrap_or_default();
    let model_id = optional(&draft);
    let label = if needs_model { "Model ID" } else { "Model ID (optional)" };
    rsx! {
        details {
            summary { "Rolling Window Starter"
                if let Some(starter) = &account.rolling_starter {
                    if starter.enabled { " · Enabled" } else { " · Off" }
                }
            }
            p { class: "form-help", "Starting a window sends a real request and can consume quota." }
            if let Some(starter) = &account.rolling_starter {
                dl { class: "inspector-kv-grid",
                    div { dt { "Active window started" } dd { "{date(starter.active_window_started_at)}" } }
                    div { dt { "Last start" } dd { "{date(starter.last_started_at)}" } }
                    div { dt { "Next scheduled start" } dd { "{date(starter.next_scheduled_start_at)}" } }
                }
                if let Some(error) = &starter.last_error { p { "{error}" } }
            } else {
                p { class: "form-help", "No starter status reported." }
            }
            if can_start {
                div { class: "row",
                    Field { label, value: draft, change: move |v| model.set(Some(v)) }
                    OperationButton { provider, account: account.clone(), label: "Start Window", confirm: true,
                        disabled: !account.enabled || (needs_model && model_id.is_none()),
                        command: ProviderCommand::Quota(QuotaCommand::TriggerRollingStart { account_id: account.id.clone(), model_id }) }
                }
            }
        }
    }
}

#[component]
fn AccountQuotaHistory(provider_id: String, account: ModelAccount, lookback_default: u32, horizon_default: u32) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut lookback = use_signal(|| if [3, 6, 12, 24].contains(&lookback_default) { lookback_default } else { 6 });
    let mut horizon = use_signal(|| if [6, 12, 24, 48].contains(&horizon_default) { horizon_default } else { 12 });
    let mut range = use_signal(|| 24u32);
    let mut show_forecast = use_signal(|| false);
    let mut window = use_signal(String::new);
    let mut scopes = BTreeMap::<&str, (&str, BTreeSet<Option<&str>>)>::new();
    for w in &account.windows {
        scopes.entry(&w.window_key).or_insert_with(|| (&w.label, BTreeSet::new())).1.insert(w.scope_model.as_deref());
    }
    let windows: BTreeMap<_, _> = scopes
        .into_iter()
        .map(|(key, (label, models))| {
            (key.to_string(), if models.len() > 1 { format!("{key} · model scopes") } else { window_label(label).to_string() })
        })
        .collect();
    let chosen = window.cloned();
    let chosen = windows.get_key_value(&chosen).or_else(|| windows.first_key_value()).map(|(key, _)| key.clone());
    let account_id = account.id.clone();
    let query = use_memo(use_reactive((&chosen, &account_id), move |(window_key, account_id)| QuotaQuery {
        account_id: Some(account_id),
        window_key,
        range_hours: Some(range()),
        lookback_hours: Some(lookback()),
        horizon_hours: Some(horizon()),
    }));
    let id = segment(&provider_id);
    let data = use_resource(move || {
        let _ = (panel.epoch)();
        let query = query();
        let (id, client) = (id.clone(), client.clone());
        async move { timed(client.provider_quota(&path_id(id)?, &query)).await.map(|report| (query, report)) }
    });
    let current_query = query();
    let result = data.read();
    let current_report =
        result.as_ref().and_then(|r| r.as_ref().ok()).filter(|(submitted, _)| *submitted == current_query).map(|(_, report)| report);
    let matches_account = |id: &str| id == account.id;
    let plot_window = chosen
        .as_deref()
        .or_else(|| current_report?.samples.iter().find(|s| matches_account(&s.account_id)).map(|s| s.window_key.as_str()));
    let matches_forecast =
        |f: &&QuotaForecast| f.account_id.as_deref().is_some_and(matches_account) && plot_window == Some(f.window_key.as_str());
    let available = current_report.is_some_and(|report| {
        report
            .forecasts
            .iter()
            .filter(matches_forecast)
            .any(|f| f.points.iter().any(|p| p.at > report.as_of && p.remaining_percent.is_finite()))
    });
    let future = available && show_forecast();
    let name = account_name(&account, panel.settings.read().mask_emails);
    rsx! {
        div { class: "form-grid-2 quota-controls",
            if let Some(chosen) = &chosen {
                Choice { label: "Window", value: chosen.clone(), options: windows.into_iter().collect(), change: move |v| window.set(v) }
            }
            Choice { label: "History", value: range().to_string(), options: pairs(&[("6", "6h"), ("12", "12h"), ("24", "24h"), ("48", "48h"), ("168", "7d")]),
                change: move |v: String| if let Ok(n) = v.parse() { range.set(n); } }
            Choice { label: "Drain lookback", value: lookback().to_string(), options: pairs(LOOKBACK),
                change: move |v: String| if let Ok(n) = v.parse() { lookback.set(n); } }
            Choice { label: "Forecast horizon", value: horizon().to_string(), options: pairs(HORIZON),
                change: move |v: String| if let Ok(n) = v.parse() { horizon.set(n); } }
            div { class: "provider-field quota-forecast-toggle",
                Check { label: "Show forecast", value: future, disabled: !available, change: move |value| show_forecast.set(value) }
                if !available { small { "No forecast available" } }
            }
        }
        {read_view(result.as_ref(), |_| {
            let Some(report) = current_report else { return rsx! { Loading { text: "Loading quota range…".to_string() } }; };
            let history = TimeBounds { start: report.as_of - TimeDelta::hours(range().into()), end: report.as_of };
            rsx! {
                if let Some(chosen) = plot_window {
                    QuotaHistoryChart { key: "{chosen}-{range()}-{lookback()}-{horizon()}",
                        samples: report.samples.iter().filter(|s| matches_account(&s.account_id)).cloned().collect(),
                        forecasts: report.forecasts.iter().filter(matches_forecast).cloned().collect(),
                        names: BTreeMap::from([(account.id.clone(), name.clone())]),
                        current: report.accounts.iter().filter(|a| matches_account(&a.id)).cloned().collect(), window_key: chosen.to_string(),
                        scope: QuotaChartScope {
                            history, forecast_until: future.then(|| report.as_of + TimeDelta::hours(horizon().into())),
                            history_truncated: report.history_truncated, pool: false,
                        }
                    }
                } else {
                    p { class: "muted", "No quota window observed yet." }
                }
                details { summary { "Past Windows" }
                    Table { label: "Past windows", headers: &["Account", "Window", "Model", "Started", "Ended", "Peak used %", "Exhausted", "Early replacement", "Samples"],
                        rows: report.past_windows.iter().rev().filter(|w| matches_account(&w.account_id)).map(|w| vec![
                            name.clone(), window_label(&w.label).to_string(), w.scope_model.clone().unwrap_or_else(|| "—".into()),
                            fmt_dt(w.started_at), fmt_dt(w.ended_at), number(w.peak_used_percent), date(w.exhausted_at),
                            if w.early_replaced { "Yes" } else { "No" }.into(), w.sample_count.to_string()
                        ]).collect() }
                }
            }
        })}
    }
}

#[component]
fn KeyGroups(provider: ProviderSummary) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let id = segment(&provider.descriptor.id);
    let cid = id.clone();
    let model_client = client.clone();
    let data = use_resource(move || {
        let (id, client) = (cid.clone(), client.clone());
        let _ = (panel.epoch)();
        async move { timed(client.provider_key_groups(&path_id(id)?)).await }
    });
    let models = use_resource(move || {
        let (id, client) = (id.clone(), model_client.clone());
        let _ = (panel.epoch)();
        async move { timed(client.provider_models(&path_id(id)?)).await }
    });
    let groups = data.read();
    let models = models.read();
    let can_manage = panel.operator() && supports(&provider, None, ProviderCapability::ManageKeyGroups);
    let can_discover = panel.operator() && supports(&provider, None, ProviderCapability::DiscoverModels);
    let model_names: BTreeMap<_, _> = models
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .into_iter()
        .flatten()
        .filter_map(|model| Some((model.id.clone(), optional(model.display_name.as_deref()?)?)))
        .collect();
    let no_groups = groups.as_ref().is_some_and(|result| result.as_ref().is_ok_and(Vec::is_empty));
    rsx! {
        div { class: "stack",
            if !no_groups || can_manage || can_discover {
                div { class: "row",
                    h3 { "API key groups" }
                    OperationButton { provider: provider.clone(), label: "Discover models",
                        command: ProviderCommand::Model(ModelCommand::DiscoverModels { group_id: None }) }
                }
            }
            {read_view(groups.as_ref(), |groups| rsx! {
                if groups.is_empty() { p { class: "form-help", "No key groups reported." } }
                for group in groups {
                    KeyGroupView { key: "{group.id}-{group.updated_at.timestamp_millis()}", provider: provider.clone(), group: group.clone(), model_names: model_names.clone() }
                }
                if can_manage {
                    details {
                        summary { "Add key group" }
                        GroupEditor { provider: provider.clone(), group: None }
                    }
                }
            })}
            if !no_groups || !models.as_ref().is_some_and(|result| result.as_ref().is_ok_and(Vec::is_empty)) {
                details { summary { "Provider model inventory" }
                    {read_view(models.as_ref(), |models| rsx! { ModelList { models: models.iter().map(|m| m.id.clone()).collect() } })}
                }
            }
        }
    }
}

#[component]
fn ModelList(models: Vec<String>) -> Element {
    if models.is_empty() {
        return rsx! { p { class: "muted", "No model IDs reported." } };
    }
    rsx! { Output { label: "Model IDs (one per line)", value: models.join("\n") } }
}

#[component]
fn KeyGroupView(provider: ProviderSummary, group: KeyGroup, model_names: BTreeMap<String, String>) -> Element {
    let mut key_label = use_signal(String::new);
    let mut key_ref = use_signal(String::new);
    let mut models = use_signal(|| group.configured_models.join("\n"));
    let mut aliases =
        use_signal(|| group.aliases.iter().map(|a| format!("{}={}", a.alias, a.target_model_id)).collect::<Vec<_>>().join("\n"));
    let id = group.id.clone();
    let enabled_cmd = KeyGroupCommand::SetGroupEnabled { group_id: id.clone(), enabled: !group.enabled };
    let mode = match group.request_identity {
        RequestIdentityMode::Api => RequestIdentityMode::AutoClient,
        RequestIdentityMode::AutoClient => RequestIdentityMode::Api,
    };
    let parsed_aliases = parse_aliases(&aliases());
    let group_action = if group.enabled { "Disable group" } else { "Enable group" };
    let identity_action = if mode == RequestIdentityMode::Api { "Use API identity" } else { "Use automatic client identity" };
    rsx! {
        article { class: "provider-inset stack",
            h3 { "{group.name}" }
            p { class: "muted", {format!("{} · {} · {} · {}", group.family, group.base_url.as_deref().unwrap_or("default endpoint"), request_identity_label(group.request_identity), if group.enabled { "Enabled" } else { "Disabled" })} }
            div { class: "row",
                OperationButton { provider: provider.clone(), label: group_action,
                    command: ProviderCommand::KeyGroup(enabled_cmd) }
                OperationButton { provider: provider.clone(), label: identity_action,
                    command: ProviderCommand::KeyGroup(KeyGroupCommand::SetRequestIdentity { group_id: id.clone(), mode }) }
                OperationButton { provider: provider.clone(), label: "Discover Models",
                    command: ProviderCommand::Model(ModelCommand::DiscoverModels { group_id: Some(id.clone()) }) }
                OperationButton { provider: provider.clone(), label: "Delete group", tone: "danger", confirm: true,
                    command: ProviderCommand::KeyGroup(KeyGroupCommand::DeleteGroup { group_id: id.clone() }) }
            }
            h4 { "Saved keys" }
            if group.keys.is_empty() { p { class: "muted", "No saved keys." } }
            for key in &group.keys {
                {let key_action = if key.enabled { "Disable key" } else { "Enable key" }; rsx! {
                div { key: "{key.id}", class: "row",
                    span { "{key.label} · {key.key_hint} · " if key.enabled { "Enabled" } else { "Disabled" } " · {key.success_count} successful / {key.failure_count} failed" }
                    OperationButton { provider: provider.clone(), label: key_action,
                        command: ProviderCommand::KeyGroup(KeyGroupCommand::ToggleKey { group_id: id.clone(), key_id: key.id.clone(), enabled: !key.enabled }) }
                    OperationButton { provider: provider.clone(), label: "Remove key", confirm: true,
                        command: ProviderCommand::KeyGroup(KeyGroupCommand::RemoveKey { group_id: id.clone(), key_id: key.id.clone() }) }
                }
                }}
            }
            if supports(&provider, None, ProviderCapability::ManageKeys) {
                div { class: "row",
                    Field { label: "Key label", value: key_label(), change: move |v| key_label.set(v) }
                    Field { label: "Secret reference (not the key)", kind: "password", value: key_ref(), change: move |v| key_ref.set(v) }
                    OperationButton { provider: provider.clone(), label: "Add key", disabled: optional(&key_label()).is_none() || optional(&key_ref()).is_none(),
                        command: ProviderCommand::KeyGroup(KeyGroupCommand::AddKey { group_id: id.clone(), label: key_label().trim().into(), key_ref: key_ref().trim().into() }) }
                }
            }
            details { summary { "Configured models and aliases" }
                ModelList { models: group.configured_models.clone() }
                for alias in &group.aliases {
                    {let model = group.discovered_models.iter().find(|m| m.id == alias.target_model_id)
                        .and_then(|m| m.display_name.as_deref()).filter(|name| !name.trim().is_empty())
                        .or_else(|| model_names.get(&alias.target_model_id).map(String::as_str));
                    rsx! { p {
                        "{alias.alias} → "
                        if let Some(model) = model.filter(|name| *name != alias.target_model_id) {
                            "{model} " code { class: "muted", "({alias.target_model_id})" }
                        } else { code { "{alias.target_model_id}" } }
                    } }}
                }
                if supports(&provider, None, ProviderCapability::ConfigureModels) {
                    label { class: "provider-field", "Configured models, one ID per line",
                        textarea { class: "input", value: models(), oninput: move |e| models.set(e.value()) }
                    }
                    OperationButton { provider: provider.clone(), label: "Save models",
                        command: ProviderCommand::Model(ModelCommand::SetModels { group_id: id.clone(), models: lines(&models()) }) }
                }
                if supports(&provider, None, ProviderCapability::SetModelAliases) {
                    label { class: "provider-field", "Aliases, one alias=model per line",
                        textarea { class: "input", value: aliases(), oninput: move |e| aliases.set(e.value()) }
                    }
                    OperationButton { provider: provider.clone(), label: "Save aliases", disabled: parsed_aliases.is_none(),
                        command: ProviderCommand::Model(ModelCommand::SetAliases { group_id: id, aliases: parsed_aliases.unwrap_or_default() }) }
                }
            }
            details { summary { "Discovered models ({group.discovered_models.len()})" }
                ModelList { models: group.discovered_models.iter().map(|m| m.id.clone()).collect() }
            }
            if supports(&provider, None, ProviderCapability::ManageKeyGroups) {
                details { summary { "Edit key group" }
                    GroupEditor { provider, group: Some(group) }
                }
            }
        }
    }
}

fn lines(value: &str) -> Vec<String> {
    value.lines().filter_map(optional).collect()
}

fn parse_aliases(value: &str) -> Option<Vec<ModelAlias>> {
    lines(value)
        .iter()
        .map(|line| {
            let (alias, model) = line.split_once('=')?;
            Some(ModelAlias { alias: optional(alias)?, target_model_id: optional(model)? })
        })
        .collect()
}

#[component]
fn GroupEditor(provider: ProviderSummary, group: Option<KeyGroup>) -> Element {
    let mut name = use_signal(|| group.as_ref().map(|g| g.name.clone()).unwrap_or_default());
    let mut family = use_signal(|| group.as_ref().map_or_else(|| provider.descriptor.family.clone(), |g| g.family.clone()));
    let mut url = use_signal(|| group.as_ref().and_then(|g| g.base_url.clone()).unwrap_or_default());
    let base_url = optional(&url());
    let valid = !name().trim().is_empty()
        && !family().trim().is_empty()
        && base_url.as_deref().is_none_or(|s| s.starts_with("https://") || s.starts_with("http://"));
    let command = KeyGroupCommand::UpsertGroup {
        group_id: group.as_ref().map(|g| g.id.clone()),
        family: family().trim().into(),
        name: name().trim().into(),
        base_url,
        enabled: group.as_ref().is_none_or(|g| g.enabled),
        request_identity: group.as_ref().map(|g| g.request_identity).unwrap_or_default(),
        configured_models: group.as_ref().map(|g| g.configured_models.clone()).unwrap_or_default(),
        aliases: group.as_ref().map(|g| g.aliases.clone()).unwrap_or_default(),
    };
    rsx! { div { class: "row",
        Field { label: "Group name", value: name(), change: move |v| name.set(v) }
        Field { label: "Family", value: family(), change: move |v| family.set(v) }
        Field { label: "Base URL (optional)", kind: "url", value: url(), change: move |v| url.set(v) }
        OperationButton { provider, label: "Save key group", disabled: !valid, command: ProviderCommand::KeyGroup(command) }
    } }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use dioxus::dioxus_core::{AttributeValue, Mutation};

    use super::*;
    use crate::workspaces::providers::dashboard::tests::test_panel;

    #[test]
    fn account_sort_updates_when_saved_settings_arrive_after_mount() {
        #[derive(Clone, PartialEq, Props)]
        struct TestProps {
            settings: Rc<Cell<Option<Signal<ProviderPanelSettings>>>>,
        }

        fn app(TestProps { settings }: TestProps) -> Element {
            settings.set(Some(test_panel().settings));
            rsx! { Accounts { providers: vec![] } }
        }

        let settings = Rc::new(Cell::new(None));
        let mut dom = VirtualDom::new_with_props(app, TestProps { settings: settings.clone() });
        dom.rebuild_to_vec();
        let mut settings = settings.get().unwrap();
        dom.in_runtime(|| settings.write().account_sort = AccountSortOrder::MostLeft);
        let edits = dom.render_immediate_to_vec().edits;
        assert!(
            edits.iter().any(|edit| matches!(edit, Mutation::SetAttribute { name, value: AttributeValue::Text(value), .. } if name == "value" && value == "most-left")),
            "{edits:?}"
        );
    }

    #[test]
    fn sort_account_rows_orders_by_capacity_usage_reset_and_name() {
        let provider: ProviderSummary = serde_json::from_value(serde_json::json!({
            "descriptor": {"id": "test", "label": "Test", "family": "test", "mode": "fixture", "verification": "built"},
            "account_count": 5, "enabled_accounts": 5, "pools": [],
            "next_pick_account_id": "alpha",
            "accounts": [
                {"id": "zulu", "provider_id": "test", "source_account_id": "zulu", "display_name": "Zulu",
                 "tier_multiplier": 20.0, "windows": [
                    {"window_key": "weekly", "label": "Weekly", "kind": "weekly", "observation_state": "measured",
                     "remaining_percent": 80.0, "resets_at": "2030-01-02T00:00:00Z"},
                    {"window_key": "spend", "label": "Spend", "kind": "spend", "observation_state": "measured",
                     "remaining_percent": 0.0, "resets_at": "2030-01-01T00:00:00Z"}
                 ]},
                {"id": "bravo", "provider_id": "test", "source_account_id": "bravo", "display_name": "Bravo", "is_active": true,
                 "windows": [{"window_key": "weekly", "label": "Weekly", "kind": "weekly", "observation_state": "measured",
                              "remaining_percent": 20.0, "resets_at": "2030-01-03T00:00:00Z"}]},
                {"id": "alpha", "provider_id": "test", "source_account_id": "alpha", "display_name": "alpha",
                 "windows": [{"window_key": "weekly", "label": "Weekly", "kind": "weekly", "observation_state": "measured",
                              "resets_at": "2020-01-01T00:00:00Z"}]},
                {"id": "charlie", "provider_id": "test", "source_account_id": "charlie", "display_name": "Charlie", "priority": 1},
                {"id": "delta", "provider_id": "test", "source_account_id": "delta", "display_name": "Delta",
                 "priority": 10}
            ]
        }))
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2029-12-31T00:00:00Z").unwrap().with_timezone(&Utc);
        for (sort, expected) in [
            (AccountListSort::Capacity, ["zulu", "bravo", "alpha", "delta", "charlie"]),
            (AccountListSort::Usage, ["bravo", "zulu", "alpha", "charlie", "delta"]),
            (AccountListSort::from_setting(AccountSortOrder::MostLeft), ["zulu", "bravo", "alpha", "charlie", "delta"]),
            (AccountListSort::NextReset, ["zulu", "bravo", "alpha", "charlie", "delta"]),
            (AccountListSort::Name, ["alpha", "bravo", "charlie", "delta", "zulu"]),
        ] {
            let mut rows = provider.accounts.iter().map(|account| (&provider, account)).collect();
            sort_account_rows_at(&mut rows, sort, now);
            assert_eq!(rows.iter().map(|(_, account)| account.id.as_str()).collect::<Vec<_>>(), expected);
        }
    }

    #[test]
    fn pool_share_percent_computes_tier_share() {
        assert_eq!(pool_share_percent(Some(20.0), Some(27.0)).map(|p| p.round()), Some(74.0));
        assert_eq!(pool_share_percent(Some(1.0), Some(1.0)), Some(100.0));
        for (tier, total) in [(None, Some(27.0)), (Some(f64::NAN), Some(27.0)), (Some(1.0), Some(0.0)), (Some(1.0), None)] {
            assert_eq!(pool_share_percent(tier, total), None);
        }
    }
}
