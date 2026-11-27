#[cfg(test)]
use std::cmp::Ordering;
use std::rc::Rc;

use chrono::TimeDelta;

use super::{charts::*, *};

pub(super) const LOOKBACK: &[(&str, &str)] = &[("3", "3h"), ("6", "6h"), ("12", "12h"), ("24", "24h")];
pub(super) const HORIZON: &[(&str, &str)] = &[("6", "6h"), ("12", "12h"), ("24", "24h"), ("48", "48h")];
pub(super) const NEXT_PICK_HINT: &str = "Next pick for routing (cached; not model-scoped)";

#[component]
pub(super) fn Dashboard(providers: Vec<ProviderSummary>, stale: bool, all_providers: bool) -> Element {
    let panel = use_context::<PanelState>();
    if let Some(id) = (panel.detail_provider)()
        && let Some(provider) = providers.iter().find(|p| p.descriptor.id == id)
    {
        return rsx! { super::provider_detail::ProviderDetail { key: "{id}", provider: provider.clone(), stale } };
    }
    rsx! { Cockpit { providers, stale, all_providers } }
}

#[component]
fn Cockpit(providers: Vec<ProviderSummary>, stale: bool, all_providers: bool) -> Element {
    let panel = use_context::<PanelState>();
    let client = use_context::<HubClient>();
    let inventory_client = client.clone();
    // The workspace passes the filtered rows; global controls still need the full inventory.
    let inventory = use_resource(use_reactive((&all_providers,), move |(all_providers,)| {
        let _ = (panel.epoch)();
        let client = inventory_client.clone();
        async move { if all_providers { None } else { Some(timed(client.providers()).await) } }
    }));
    let global = inventory.read();
    let global = global.as_ref().and_then(Option::as_ref);
    let inventory = if all_providers { Some(providers.as_slice()) } else { global.and_then(|r| r.as_ref().ok().map(Vec::as_slice)) };
    let totals = inventory.unwrap_or(&providers);
    let enabled: usize = totals.iter().map(|p| p.enabled_accounts).sum();
    let attention = totals.iter().filter(|p| needs_attention(p)).count();
    let refreshable: Vec<_> = inventory
        .into_iter()
        .flatten()
        .filter(|p| supports(p, None, ProviderCapability::RefreshQuota))
        .map(|p| p.descriptor.id.clone())
        .collect();
    let mut legend = use_signal(|| false);
    let scoped_provider = if all_providers { None } else { optional(&(panel.selected_provider)()) };
    rsx! {
        header { class: "cockpit-header row",
            div {
                h1 { "Provider Operations" }
                p { class: "muted", "{totals.len()} providers · {enabled} enabled accounts" }
            }
            div { class: "row",
                if attention > 0 {
                    Chip { label: format!("{attention} attention"), tone: Some("warn".into()) }
                }
                button { class: "btn ghost btn-xs", aria_label: "Quota meter legend", aria_expanded: legend(),
                    onclick: move |_| legend.toggle(), "?" }
                if panel.operator() && !refreshable.is_empty() {
                    button { class: "btn secondary", disabled: stale || (panel.busy)(),
                        onclick: move |_| {
                            let Some(busy) = panel.begin() else { return; };
                            let (ids, client) = (refreshable.clone(), client.clone());
                            Runtime::current().spawn(panel.scope, async move {
                                let _busy = busy;
                                let mut outcomes = Vec::with_capacity(ids.len());
                                let mut succeeded = true;
                                let mut panel = panel;
                                for id in ids {
                                    execute_operation(panel, &client, &id, None, None, collect_quota()).await;
                                    if let Some((ok, notice)) = panel.notice.cloned() {
                                        succeeded &= ok;
                                        outcomes.push(format!("{id}: {notice}"));
                                    }
                                }
                                panel.notice.set(Some((succeeded, outcomes.join(" · "))));
                            });
                        },
                        "Refresh all"
                    }
                }
            }
        }
        if legend() {
            div { class: "cockpit-legend-popover glass-card stack",
                super::QuotaMeterKey {}
                button { class: "btn ghost btn-xs", onclick: move |_| panel.open_settings(), "Full reference in Settings & Help →" }
            }
        }
        if global.is_some_and(Result::is_err) {
            p { class: "form-help", role: "status", "Full provider inventory unavailable. Showing the selected provider only." }
        }
        div { class: "cockpit-grid",
            section { class: "cockpit-col stack", aria_label: "Provider headroom",
                if providers.is_empty() { p { class: "muted", "No providers reported." } }
                for provider in &providers {
                    ProviderCard { key: "{provider.descriptor.id}", provider: provider.clone(), stale }
                }
                CockpitRouting { providers: totals.to_vec() }
            }
            CockpitActivity { key: "{scoped_provider:?}", provider_id: scoped_provider }
        }
    }
}

pub(super) fn collect_quota() -> ProviderCommand {
    ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id: None })
}

pub(super) fn has_quota(provider: &ProviderSummary) -> bool {
    provider.descriptor.evidence_kinds.contains(&EvidenceKind::QuotaWindows)
        || !provider.pools.is_empty()
        || provider.accounts.iter().any(|a| !a.windows.is_empty())
}

fn account_attention(account: &ModelAccount) -> bool {
    account_status(account, false) == AccountStatus::Error || account.last_error.is_some()
}

fn needs_attention(provider: &ProviderSummary) -> bool {
    provider.descriptor.health != ProviderHealth::Healthy || provider.accounts.iter().any(account_attention)
}

pub(super) fn pool_window_label(pool: &PoolWindowSummary) -> String {
    let label = if pool.label.trim().is_empty() { &pool.window_key } else { &pool.label };
    let label = window_label(label);
    pool.scope_model.as_ref().map_or_else(|| label.into(), |model| format!("{label} · {model}"))
}

fn enabled_pool_accounts(provider: &ProviderSummary) -> impl Iterator<Item = &ModelAccount> {
    provider.accounts.iter().filter(|a| a.enabled && provider.routed_account_ids.as_ref().is_none_or(|ids| ids.contains(&a.id)))
}

pub(super) fn pool_tiers(provider: &ProviderSummary) -> Vec<(f64, &str, usize)> {
    let mut tiers = std::collections::BTreeMap::new();
    for account in enabled_pool_accounts(provider) {
        let units = account.tier_multiplier.filter(|n| n.is_finite() && *n > 0.0).unwrap_or(1.0);
        let plan = account.plan.as_deref().filter(|p| !p.trim().is_empty()).unwrap_or("");
        *tiers.entry((units.to_bits(), plan)).or_insert(0) += 1;
    }
    tiers.into_iter().rev().map(|((bits, plan), count)| (f64::from_bits(bits), plan, count)).collect()
}

pub(super) fn tier_label(units: f64, plan: &str) -> String {
    let multiplier = format_multiplier(units);
    if plan.is_empty() {
        multiplier
    } else if plan.split_whitespace().any(|word| word == multiplier) {
        plan.into()
    } else {
        format!("{multiplier} {plan}")
    }
}

pub(super) fn pool_roster_summary(provider: &ProviderSummary) -> String {
    roster_summary(provider, true)
}

fn roster_summary(provider: &ProviderSummary, mask: bool) -> String {
    if enabled_pool_accounts(provider).any(|a| a.tier_multiplier.is_some() || a.plan.as_deref().is_some_and(|p| !p.trim().is_empty())) {
        return pool_tiers(provider)
            .into_iter()
            .map(|(units, plan, count)| format!("{count}× {}", tier_label(units, plan)))
            .collect::<Vec<_>>()
            .join(" · ");
    }
    let enabled = provider.enabled_accounts;
    let next = enabled_pool_accounts(provider)
        .find(|a| a.is_next_pick || provider.next_pick_account_id.as_ref() == Some(&a.id))
        .map(|a| format!(" · next: {}", account_name(a, mask)))
        .unwrap_or_default();
    format!("{enabled} enabled{next}")
}

pub(super) struct UpcomingResetItem {
    provider: String,
    window: String,
    at: DateTime<Utc>,
}

pub(super) fn upcoming_pool_resets(providers: &[ProviderSummary], limit: usize) -> Vec<UpcomingResetItem> {
    upcoming_pool_resets_at(providers, limit, Utc::now())
}

fn upcoming_pool_resets_at(providers: &[ProviderSummary], limit: usize, now: DateTime<Utc>) -> Vec<UpcomingResetItem> {
    let mut resets: Vec<_> = providers
        .iter()
        .flat_map(|provider| {
            provider.pools.iter().filter_map(move |pool| {
                let at = pool.next_reset_at.filter(|at| *at > now)?;
                Some(UpcomingResetItem { provider: provider.descriptor.label.clone(), window: pool_window_label(pool), at })
            })
        })
        .collect();
    resets.sort_by_key(|r| r.at);
    resets.truncate(limit);
    resets
}

#[component]
fn CockpitRouting(providers: Vec<ProviderSummary>) -> Element {
    let panel = use_context::<PanelState>();
    let resets = upcoming_pool_resets(&providers, 5);
    let routed: Vec<_> = providers
        .iter()
        .filter(|p| p.descriptor.router_id.is_some() || p.descriptor.capabilities.contains(&ProviderCapability::ReadProxyStatus))
        .collect();
    rsx! {
        section { class: "glass-card stack",
            div { class: "row",
                h2 { "Upcoming Resets & Proxy Routing" }
                button { class: "btn ghost btn-xs", onclick: move |_| panel.open_routing(), "Routing & Proxy →" }
            }
            if routed.is_empty() { p { class: "muted", "No proxy routing reported." } }
            for provider in routed {
                CockpitRoute { key: "{provider.descriptor.id}", id: provider.descriptor.id.clone(), label: provider.descriptor.label.clone() }
            }
            if resets.is_empty() {
                p { class: "muted", "No upcoming pool resets reported." }
            } else {
                ul { class: "cockpit-reset-list",
                    for reset in resets {
                        li { class: "row",
                            span { "{reset.provider} · {reset.window}" }
                            span { title: "{fmt_dt(reset.at)}", ResetTime { at: Some(reset.at) } }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn CockpitRoute(id: String, label: String) -> Element {
    let panel = use_context::<PanelState>();
    let client = use_context::<HubClient>();
    let id = segment(&id);
    let data = use_resource(move || {
        let _ = (panel.epoch)();
        let (id, client) = (id.clone(), client.clone());
        async move { timed(client.provider_routing(&path_id(id)?)).await }
    });
    let result = data.read();
    read_view(result.as_ref(), |routing| {
        rsx! {
            div { class: "row",
                strong { "{label}" }
                span { "{route_strategy_label(routing.policy.strategy)}" }
                if let Some(runtime) = &routing.runtime {
                    Chip { label: if runtime.running { "Running" } else { "Stopped" },
                        tone: Some(if runtime.running { "ok" } else { "muted" }.to_string()) }
                    if let Some(endpoint) = &runtime.endpoint { span { class: "form-help", "{endpoint}" } }
                    else if let Some(port) = runtime.port { span { class: "form-help", "Port {port}" } }
                } else {
                    span { class: "form-help", "Proxy runtime not observed" }
                }
            }
        }
    })
}

#[component]
fn CockpitActivity(#[props(!optional)] provider_id: Option<String>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut hours = use_signal(|| 168_u32);
    let mut metric = use_signal(|| "cost");
    let data = use_resource(move || {
        let _ = (panel.epoch)();
        let (client, provider_id, range) = (client.clone(), provider_id.clone(), hours());
        let to = Utc::now();
        let scope = ActivityScope { from: Some(to - TimeDelta::hours(range.into())), to: Some(to), provider_id, ..Default::default() };
        async move { timed(client.activity_overview(&scope)).await.map(|overview| (range, overview)) }
    });
    let result = data.read();
    rsx! {
        section { class: "cockpit-col stack", aria_label: "Activity and efficiency",
            div { class: "row",
                h2 { "Activity & Efficiency" }
                button { class: "btn ghost btn-xs", onclick: move |_| panel.open_activity(None), "Activity →" }
            }
            div { class: "row",
                div { class: "row", role: "group", aria_label: "Activity range",
                    for (range, label) in [(24, "24h"), (168, "7d"), (720, "30d")] {
                        button { key: "{range}", class: if hours() == range { "btn secondary btn-xs" } else { "btn ghost btn-xs" },
                            aria_pressed: hours() == range, onclick: move |_| hours.set(range), "{label}" }
                    }
                }
                div { class: "row", role: "group", aria_label: "Activity metric",
                    for (value, label) in [("cost", "Cost"), ("tokens", "Tokens"), ("requests", "Requests")] {
                        button { key: "{value}", class: if metric() == value { "btn secondary btn-xs" } else { "btn ghost btn-xs" },
                            aria_pressed: metric() == value, onclick: move |_| metric.set(value), "{label}" }
                    }
                }
            }
            if data.pending() {
                Loading { text: "Loading activity…".to_string() }
            } else {
                {read_view(result.as_ref(), |(range, overview)| {
                    if *range != hours() {
                        rsx! { Loading { text: "Loading activity…".to_string() } }
                    } else if overview.requests == 0 && overview.total_tokens == 0 {
                        rsx! { p { class: "muted", "No activity in this range." } }
                    } else {
                        rsx! { ActivityDigest { overview: overview.clone(), metric: metric().to_string() } }
                    }
                })}
            }
        }
    }
}

#[component]
fn ActivityDigest(overview: ActivityOverview, metric: String) -> Element {
    let panel = use_context::<PanelState>();
    let mask = panel.settings.read().mask_emails;
    let hit = effective_cache_hit_rate(overview.input_tokens, overview.cache_read_tokens, overview.cache_hit_rate);
    let digest = [
        (
            "API-Eq Value",
            format!("${:.4}", overview.api_equivalent_cost_usd),
            format!("{} unpriced · not billed", fmt_count(overview.unpriced_requests)),
        ),
        ("Total Tokens", fmt_count(overview.total_tokens), format!("{} output", fmt_count(overview.output_tokens))),
        ("Requests", fmt_count(overview.requests), format!("{} failed", fmt_count(overview.failures))),
        ("Cache Hit Rate", fmt_hit_pct(hit), format!("{} cache-read tokens", fmt_count(overview.cache_read_tokens))),
    ];
    rsx! {
        div { class: "cockpit-kpi-grid activity-kpi-grid",
            for (label, value, sub) in digest {
                div { key: "{label}", class: "stat-card glass-card stack",
                    span { class: "form-help", "{label}" }
                    strong { class: "value", "{value}" }
                    span { class: "form-help", "{sub}" }
                }
            }
        }
        div { class: "glass-card", CockpitActivitySparkChart { overview: overview.clone(), metric } }
        div { class: "glass-card stack",
            for (label, rows) in [("Top Models", &overview.by_model), ("Top Clients", &overview.by_client)] {
                {
                    let cost: f64 = rows.iter().map(|r| r.api_equivalent_cost_usd).sum();
                    let requests: f64 = rows.iter().map(|r| r.requests as f64).sum();
                    rsx! {
                        section { key: "{label}", class: "cockpit-share-list stack",
                            h3 { "{label}" }
                            if rows.is_empty() { p { class: "form-help", "No activity in this group." } }
                            for row in super::activity::top_breakdown_rows(rows, 4) {
                                {
                                    let name = if mask && row.label.contains('@') { mask_email(&row.label) } else { row.label };
                                    let input = if row.input_tokens == 0 { row.total_tokens.saturating_sub(row.output_tokens) } else { row.input_tokens };
                                    let hit = fmt_hit_pct(effective_cache_hit_rate(input, row.cache_read_tokens, row.cache_hit_rate));
                                    let amount = format!("${:.4} · {} req", row.api_equivalent_cost_usd, fmt_count(row.requests));
                                    let (value, total, scale_label) = if cost > 0.0 {
                                        (row.api_equivalent_cost_usd, cost, "of total API-equivalent cost")
                                    } else {
                                        (row.requests as f64, requests, "of total requests")
                                    };
                                    rsx! {
                                        div { key: "{row.key}", class: "activity-share-row",
                                            div { class: "row", span { title: "{name}", "{name}" }
                                                span { class: "chip muted", title: "Cache hit rate", "{hit}" }
                                            }
                                            ShareBar { label: amount, value, total, scale_label }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            button { class: "btn ghost btn-xs", onclick: move |_| panel.open_activity(Some("breakdowns")), "Breakdowns →" }
        }
    }
}

pub(super) fn is_extra(kind: QuotaWindowKind) -> bool {
    matches!(kind, QuotaWindowKind::ModelScoped | QuotaWindowKind::Spend)
}

pub(super) fn provider_health(health: ProviderHealth, stale: bool) -> (&'static str, &'static str) {
    match health {
        ProviderHealth::Healthy if stale => ("Healthy (last known)", "muted"),
        ProviderHealth::Healthy => ("Healthy", "ok"),
        ProviderHealth::Degraded => ("Degraded", "warn"),
        ProviderHealth::Offline => ("Offline", "bad"),
        ProviderHealth::Unobserved => ("Not observed", "muted"),
    }
}

pub(super) fn pool_quota_summary(pools: &[PoolWindowSummary]) -> String {
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

pub(super) fn account_status(account: &ModelAccount, is_next_pick: bool) -> AccountStatus {
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

#[cfg(test)]
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
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let id = provider.descriptor.id.clone();
    let detail_id = id.clone();
    let accounts_id = id.clone();
    let (health, tone) = provider_health(provider.descriptor.health, stale);
    let provider_id = segment(&id);
    let has_meter = !stale && provider.pools.iter().any(|w| w.forecast.is_none() && w.next_reset_at.is_some());
    let meter_forecasts = use_resource(use_reactive((&provider_id, &has_meter), move |(provider_id, has_meter)| {
        let _ = (panel.epoch)();
        let lookback = panel.settings.read().drain_lookback_hours;
        let client = client.clone();
        async move {
            if !has_meter {
                return None;
            }
            let query = QuotaQuery { range_hours: Some(1), lookback_hours: Some(lookback), horizon_hours: Some(1), ..Default::default() };
            let report = timed(client.provider_quota(&path_id(provider_id).ok()?, &query)).await.ok()?;
            Some(Rc::new(report.forecasts))
        }
    }));
    let forecasts = meter_forecasts.read().as_ref().and_then(Clone::clone).unwrap_or_default();
    let (extra, regular): (Vec<_>, Vec<_>) = provider.pools.iter().partition(|w| is_extra(w.kind));
    let capacity = provider_total_capacity(&provider).map(|n| format!(" · {}", format_multiplier(n))).unwrap_or_default();
    let mask = panel.settings.read().mask_emails;
    let roster = if mask { pool_roster_summary(&provider) } else { roster_summary(&provider, false) };
    let has_quota = has_quota(&provider);
    rsx! {
        article { class: "glass-card cockpit-provider-card stack",
            div { class: "cockpit-provider-head row",
                h2 {
                    button { class: "btn ghost btn-xs", onclick: move |_| panel.open_provider_detail(id.clone()), "{provider.descriptor.label}" }
                }
                Chip { label: health.to_string(), tone: Some(tone.to_string()) }
                span { class: "chip muted", title: "Enabled / total accounts", "{provider.enabled_accounts}/{provider.account_count}{capacity}" }
                Freshness { observed: provider.descriptor.last_observed_at }
                if panel.operator() && supports(&provider, None, ProviderCapability::RefreshQuota) {
                    span { title: "Collect pool quota", role: "group", aria_label: "Collect pool quota",
                        OperationButton { provider: provider.clone(), label: "↻", tone: "ghost", disabled: stale, command: collect_quota() }
                    }
                }
            }
            if needs_attention(&provider) {
                div { class: "cockpit-alert-strip severity-row severity-warn",
                    if provider.descriptor.health != ProviderHealth::Healthy {
                        p { "{health}"
                            if let Some(message) = provider.descriptor.status_message.as_deref().filter(|m| !m.trim().is_empty()) {
                                " · {message}"
                            }
                        }
                    }
                    for account in provider.accounts.iter().filter(|a| account_attention(a)) {
                        p { key: "{account.id}", "{account_name(account, mask)} · "
                            {account.last_error.as_ref().map(AccountError::summary)
                                .or_else(|| account.status_message.clone()).unwrap_or_else(|| "Error observed".into())}
                        }
                    }
                }
            }
            if stale { p { class: "form-help", "Stale · showing last known inventory and pool readings." } }
            if provider.pools.is_empty() {
                p { class: "muted", if has_quota { "No pool readings yet." } else { "No quota reported" } }
            } else {
                div { class: "cockpit-window-list stack",
                    for pool in regular {
                        PoolWindow { key: "{pool.window_key}:{pool.scope_model:?}", pool: pool.clone(),
                            forecast: meter_forecast(&forecasts, None, &pool.window_key, pool.scope_model.as_deref()) }
                    }
                    if !extra.is_empty() {
                        details { summary { "Extra windows ({extra.len()})" }
                            for pool in extra {
                                PoolWindow { key: "{pool.window_key}:{pool.scope_model:?}", pool: pool.clone(),
                                    forecast: meter_forecast(&forecasts, None, &pool.window_key, pool.scope_model.as_deref()) }
                            }
                        }
                    }
                }
            }
            footer { class: "cockpit-provider-foot row",
                span { class: "form-help", title: "{pool_quota_summary(&provider.pools)}", "{roster}" }
                button { class: "btn ghost btn-xs", onclick: move |_| panel.open_provider_detail(detail_id.clone()), "Detail & History →" }
                button { class: "btn ghost btn-xs", onclick: move |_| panel.open_accounts_for(accounts_id.clone()), "Accounts ({provider.account_count}) →" }
            }
        }
    }
}

#[component]
pub(super) fn Freshness(observed: Option<DateTime<Utc>>) -> Element {
    rsx! {
        if let Some(at) = observed {
            span { class: "form-help", title: "{fmt_dt(at)}", "{format_relative_age(at, Utc::now())}" }
        } else {
            span { class: "form-help", title: "Unobserved", "Unobserved" }
        }
    }
}

pub(super) fn meter_forecast(
    forecasts: &[QuotaForecast],
    account: Option<&str>,
    window: &str,
    model: Option<&str>,
) -> Option<QuotaForecast> {
    forecasts.iter().find(|f| f.account_id.as_deref() == account && f.window_key == window && f.scope_model.as_deref() == model).cloned()
}

#[component]
pub(super) fn PoolWindow(pool: PoolWindowSummary, forecast: Option<QuotaForecast>) -> Element {
    let units = format_capacity_units(pool.remaining_capacity_units, pool.total_capacity_units);
    let label = pool_window_label(&pool);
    rsx! {
        div { class: "cockpit-window-row",
            strong { "{label}" }
            if let (Some(left), Some(used)) = quota_values(QuotaObservationState::Measured, pool.weighted_remaining_percent, pool.weighted_used_percent) {
                Meter { left, used, resets_at: pool.next_reset_at, window_seconds: pool.window_seconds, forecast: forecast.or(pool.forecast) }
            } else { span { class: "form-help", "Not observed" } }
            if let Some(units) = units {
                span { class: "cockpit-capacity-tag", title: if pool.is_estimate { "Estimated remaining / total capacity" } else { "Remaining / total capacity" },
                    "{units}" }
            }
            span { title: pool.next_reset_at.map(fmt_dt), ResetTime { at: pool.next_reset_at } }
        }
    }
}

#[component]
pub(super) fn RollingStarter(provider: ProviderSummary, #[props(default)] detail: bool) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let available = supports(&provider, None, ProviderCapability::StartRollingWindow);
    let id = segment(&provider.descriptor.id);
    let data = use_resource(use_reactive((&id, &available), move |(id, available)| {
        let client = client.clone();
        let _ = (panel.epoch)();
        async move { if available { timed(client.provider_routing(&path_id(id)?)).await.map(Some) } else { Ok(None) } }
    }));
    let current = data.read();
    let mask = panel.settings.read().mask_emails;
    rsx! {
        details { class: if detail { "glass-card detail-accordion" } else { "provider-inset" },
            summary { if detail { "Pool Rolling Window Starter" } else { "Rolling Window Starter" } }
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
pub(super) fn RollingEditor(provider: ProviderSummary, initial: RollingWindowStarterConfig, revision: i64) -> Element {
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
pub(super) mod tests {
    use super::*;

    pub(in crate::workspaces::providers) fn fixture() -> ProviderSummary {
        serde_json::from_value(serde_json::json!({
            "descriptor": {"id": "test", "label": "Test Pool", "family": "test", "mode": "fixture",
                "verification": "built", "health": "healthy"},
            "account_count": 3, "enabled_accounts": 2, "next_pick_account_id": "pro",
            "accounts": [
                {"id": "ultra", "provider_id": "test", "source_account_id": "ultra", "display_name": "Ultra Login",
                 "plan": "Ultra", "tier_multiplier": 20},
                {"id": "pro", "provider_id": "test", "source_account_id": "pro", "display_name": "Pro Login", "plan": "Pro"},
                {"id": "off", "provider_id": "test", "source_account_id": "off", "display_name": "Disabled Login",
                 "plan": "Ultra", "tier_multiplier": 20, "enabled": false}
            ],
            "pools": [{
                "provider_id": "test", "window_key": "weekly", "label": "Weekly", "kind": "weekly",
                "eligible_accounts": 2, "included_accounts": 2, "weighted_remaining_percent": 0.0,
                "remaining_capacity_units": 0.0, "total_capacity_units": 21.0,
                "capacity_basis": "plan_multiplier", "is_estimate": true
            }]
        }))
        .unwrap()
    }

    #[test]
    fn pool_roster_summary_groups_tiers_and_falls_back_cleanly() {
        let mut provider = fixture();
        assert_eq!(pool_roster_summary(&provider), "1× 20× Ultra · 1× 1× Pro");
        for account in &mut provider.accounts {
            account.plan = None;
            account.tier_multiplier = None;
        }
        assert_eq!(pool_roster_summary(&provider), "2 enabled · next: Pro Login");
        provider.next_pick_account_id = None;
        assert_eq!(pool_roster_summary(&provider), "2 enabled");
    }

    #[test]
    fn upcoming_pool_resets_sorts_soonest_first_and_limits() {
        let mut provider = fixture();
        let now = DateTime::UNIX_EPOCH;
        for hours in [3, -1, 1, 2] {
            let mut pool = provider.pools[0].clone();
            pool.next_reset_at = Some(now + TimeDelta::hours(hours));
            provider.pools.push(pool);
        }
        let resets = upcoming_pool_resets_at(&[provider], 2, now);
        assert_eq!(resets.iter().map(|r| r.at).collect::<Vec<_>>(), [now + TimeDelta::hours(1), now + TimeDelta::hours(2)]);
    }

    #[test]
    fn dashboard_cockpit_renders_without_per_account_selection_or_inventory() {
        fn app() -> Element {
            test_panel();
            rsx! { Dashboard { providers: vec![fixture()], stale: false, all_providers: true } }
        }
        let html = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        for text in ["Provider Operations", "Test Pool", "Detail & History →", "Accounts (3) →", "0.0 / 21.0×"] {
            assert!(html.contains(text), "missing {text}");
        }
        for text in ["Back to Pool", "per-account", "Ultra Login", "Disabled Login", "Collect pool quota", "Refresh all"] {
            assert!(!html.contains(text), "unexpected {text}");
        }
    }

    pub(in crate::workspaces::providers) fn test_panel() -> PanelState {
        use_context_provider(|| HubClient::new("http://127.0.0.1:1", ""));
        use_context_provider(|| {
            let status: SystemStatus = serde_json::from_value(serde_json::json!({
                "api_version": "v1", "fixture_mode": true, "production_paused": false,
                "integrations": [], "feeds": [], "sync_failures": [],
                "principal": {"id": Uuid::nil(), "name": "test", "role": "operator"}
            }))
            .unwrap();
            PanelState {
                tab: Signal::new(0),
                selected_provider: Signal::new(String::new()),
                detail_provider: Signal::new(None),
                activity_subtab: Signal::new(None),
                settings_ready: Signal::new(false),
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
        })
    }

    #[test]
    fn pool_summary_uses_the_lowest_observed_regular_window() {
        let session = PoolWindowSummary {
            provider_id: "test".into(),
            window_key: "session".into(),
            scope_model: None,
            label: "Session".into(),
            kind: QuotaWindowKind::Session,
            eligible_accounts: 1,
            included_accounts: 1,
            weighted_used_percent: Some(65.0),
            weighted_remaining_percent: None,
            remaining_capacity_units: None,
            total_capacity_units: None,
            capacity_basis: "equal_entitlements".into(),
            next_reset_at: None,
            window_seconds: None,
            forecast: None,
            is_estimate: true,
        };
        let weekly = PoolWindowSummary { kind: QuotaWindowKind::Weekly, weighted_remaining_percent: Some(70.0), ..session.clone() };
        let spend = PoolWindowSummary { kind: QuotaWindowKind::Spend, weighted_remaining_percent: Some(0.0), ..session.clone() };
        assert_eq!(pool_quota_summary(&[weekly, spend, session]), "≈35.0% left (Session)");
    }

    #[test]
    fn healthy_account_with_last_error_observed_does_not_trigger_attention_alert() {
        let mut provider = fixture();
        provider.accounts[0].status = AccountStatus::Ready;
        provider.accounts[0].last_error = None;
        provider.accounts[0].last_error_observed = true;
        assert!(!account_attention(&provider.accounts[0]));
        assert!(!needs_attention(&provider));

        fn app() -> Element {
            test_panel();
            let mut provider = fixture();
            provider.accounts[0].status = AccountStatus::Ready;
            provider.accounts[0].last_error = None;
            provider.accounts[0].last_error_observed = true;
            rsx! { Dashboard { providers: vec![provider], stale: false, all_providers: true } }
        }
        let html = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(!html.contains("cockpit-alert-strip"), "healthy account should not render alert strip");
    }
}
