use std::collections::BTreeMap;

use chrono::TimeDelta;

use super::{
    charts::{QuotaChartScope, QuotaHistoryChart, TimeBounds, UnifiedPoolQuotaChart, window_label},
    dashboard::{
        Freshness, HORIZON, LOOKBACK, RollingStarter, account_name, collect_quota, has_quota, pool_tiers, provider_health, tier_label,
    },
    *,
};

#[derive(Clone, Copy, PartialEq)]
struct Tuning {
    range_hours: Signal<u32>,
    lookback_hours: Signal<u32>,
    horizon_hours: Signal<u32>,
    show_forecast: Signal<bool>,
}

fn use_tuning(settings: &ProviderPanelSettings) -> Tuning {
    Tuning {
        range_hours: use_signal(|| 24),
        lookback_hours: use_signal(|| settings.drain_lookback_hours),
        horizon_hours: use_signal(|| settings.forecast_horizon_hours),
        show_forecast: use_signal(|| true),
    }
}

#[component]
pub(super) fn ProviderDetail(provider: ProviderSummary, stale: bool) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let tuning = use_tuning(&panel.settings.peek());
    let mut cached_report = use_signal(|| None::<ProviderQuotaReport>);
    let id = segment(&provider.descriptor.id);
    let provider_id = provider.descriptor.id.clone();
    let quota = has_quota(&provider);
    let query = move || QuotaQuery {
        account_id: None,
        window_key: None,
        range_hours: Some((tuning.range_hours)()),
        lookback_hours: Some((tuning.lookback_hours)()),
        horizon_hours: Some((tuning.horizon_hours)()),
    };
    let data = use_resource(use_reactive((&id, &quota, &provider_id), move |(id, quota, provider_id)| {
        let _ = (panel.epoch)();
        let query = query();
        let client = client.clone();
        async move {
            if !quota {
                return None;
            }
            let report = async { timed(client.provider_quota(&path_id(id)?, &query)).await }.await;
            if let Ok(ok_report) = &report
                && ok_report.provider.id == provider_id
            {
                cached_report.set(Some(ok_report.clone()));
            }
            Some((query, report))
        }
    }));
    let current_query = query();
    let result = data.read();
    let latest =
        result.as_ref().and_then(Option::as_ref).filter(|(submitted, _)| *submitted == current_query).map(|(_, report)| report.clone());
    let loading = quota && (data.pending() || latest.is_none());
    let cached = cached_report.cloned().filter(|r| r.provider.id == provider.descriptor.id);
    let (report, error) = match latest {
        Some(Ok(ok)) => (Some(ok), None),
        Some(Err(err)) => (cached, Some(err)),
        None => (cached, None),
    };
    rsx! { DetailContent { provider, stale, tuning, report, error, loading } }
}

#[component]
fn DetailContent(
    provider: ProviderSummary,
    stale: bool,
    tuning: Tuning,
    #[props(!optional)] report: Option<ProviderQuotaReport>,
    #[props(!optional)] error: Option<UiError>,
    loading: bool,
) -> Element {
    let panel = use_context::<PanelState>();
    let id = provider.descriptor.id.clone();
    let (health, tone) = provider_health(provider.descriptor.health, stale);
    let count = provider.account_count;
    let capacity = provider_total_capacity(&provider);
    let starter = provider.descriptor.capabilities.contains(&ProviderCapability::StartRollingWindow)
        || provider.accounts.iter().any(|a| a.rolling_starter.is_some());
    let has_quota = has_quota(&provider);
    let loaded = report.as_ref();
    let pools = loaded.map_or(provider.pools.as_slice(), |report| report.pools.as_slice());
    let Tuning { range_hours, mut lookback_hours, mut horizon_hours, mut show_forecast } = tuning;
    let projection = if show_forecast() { "On" } else { "Off" };
    rsx! {
        div { class: "stack",
            header { class: "detail-header row",
                button { class: "btn ghost", onclick: move |_| panel.back_to_overview(), "← Overview" }
                h1 { "{provider.descriptor.label}" }
                Chip { label: health.to_string(), tone: Some(tone.to_string()) }
                span { class: "chip muted", "{provider.enabled_accounts}/{count} accounts" }
                if let Some(capacity) = capacity {
                    span { class: "chip info", "{format_multiplier(capacity)} pool capacity" }
                }
                Freshness { observed: provider.descriptor.last_observed_at }
                OperationButton { provider: provider.clone(), label: "Collect pool quota", disabled: stale, command: collect_quota() }
                button { class: "btn secondary", onclick: move |_| panel.open_accounts_for(id.clone()), "View {count} accounts →" }
            }
            if let Some(message) = &provider.descriptor.status_message {
                p { class: "form-help", "{message}" }
            }
            TierComposition { provider: provider.clone() }
            if stale { p { class: "severity-row severity-warn", "Stale · showing last known provider inventory." } }
            if has_quota {
                section { class: "glass-card stack", aria_label: "Pool quota history and forecast",
                    if loading && loaded.is_some() {
                        div { class: "row",
                            span { class: "chip muted", role: "status", "Updating…" }
                        }
                    }
                    if let Some(err) = &error {
                        ErrorCard { code: err.code.clone(), message: err.message.clone() }
                    }
                    if let Some(data) = loaded {
                        if data.history_truncated {
                            p { class: "form-help", "History was truncated. Showing the newest available samples." }
                        }
                        UnifiedPoolQuotaChart { report: data.clone(), range_hours, show_forecast }
                    } else if error.is_none() {
                        Loading { text: "Loading…".to_string() }
                    }
                }
                details { class: "glass-card detail-accordion",
                    summary { "Forecast & Chart Tuning"
                        span { class: "form-help", " · Lookback {lookback_hours()}h · Horizon {horizon_hours()}h · Projection {projection}" }
                    }
                    div { class: "detail-tuning-grid form-grid-2",
                        Choice { label: "Drain lookback", value: lookback_hours().to_string(), options: pairs(LOOKBACK),
                            change: move |value: String| if let Ok(hours) = value.parse() { lookback_hours.set(hours); } }
                        Choice { label: "Forecast horizon", value: horizon_hours().to_string(), options: pairs(HORIZON),
                            change: move |value: String| if let Ok(hours) = value.parse() { horizon_hours.set(hours); } }
                        Check { label: "Projection", value: show_forecast(), change: move |value| show_forecast.set(value) }
                        super::QuotaMeterKey {}
                    }
                    PoolMetadata { pools: pools.to_vec() }
                }
                details { class: "glass-card detail-accordion",
                    summary { if let Some(data) = loaded { "Past Windows ({data.past_windows.len()})" } else { "Past Windows" } }
                    if let Some(data) = loaded {
                        PastWindows { report: data.clone() }
                    } else if let Some(err) = &error {
                        ErrorCard { code: err.code.clone(), message: err.message.clone() }
                    } else {
                        Loading { text: "Loading…".to_string() }
                    }
                }
                details { class: "glass-card detail-accordion",
                    summary { if let Some(data) = loaded { "Raw Quota Data Tables ({data.samples.len()} samples)" } else { "Raw Quota Data Tables" } }
                    if let Some(data) = loaded {
                        RawQuota { report: data.clone(), tuning }
                    } else if let Some(err) = &error {
                        ErrorCard { code: err.code.clone(), message: err.message.clone() }
                    } else {
                        Loading { text: "Loading…".to_string() }
                    }
                }
            } else {
                section { class: "glass-card stack", p { class: "muted", "No quota reported." } }
            }
            if starter {
                RollingStarter { provider, detail: true }
            }
        }
    }
}

#[component]
fn TierComposition(provider: ProviderSummary) -> Element {
    let tiers = pool_tiers(&provider);
    if tiers.len() < 2
        && !provider
            .accounts
            .iter()
            .any(|a| a.enabled && a.tier_multiplier.is_some() && provider.routed_account_ids.as_ref().is_none_or(|ids| ids.contains(&a.id)))
    {
        return rsx! {};
    }
    let total: f64 = tiers.iter().map(|(units, _, count)| units * *count as f64).sum();
    if !total.is_finite() || total <= 0.0 {
        return rsx! {};
    }
    let rows: Vec<_> = tiers
        .into_iter()
        .enumerate()
        .map(|(index, (units, plan, count))| {
            let label = tier_label(units, plan);
            let share = units * count as f64 / total * 100.0;
            let color = ["var(--chart-1)", "var(--chart-2)", "var(--chart-3)", "var(--chart-4)"][index % 4];
            (index, label, share, color, count)
        })
        .collect();
    rsx! {
        section { class: "glass-card stack", aria_label: "Tier composition",
            div { class: "row", h2 { "Tier composition" } span { class: "chip info", "{format_multiplier(total)} total" } }
            div { class: "tier-comp-bar", aria_hidden: "true", style: "display:flex;",
                for (index, _, share, color, _) in &rows {
                    span { key: "{index}", style: "background:{color}; width:{share}%; min-height:0.5rem;" }
                }
            }
            ul { class: "row",
                for (index, label, share, color, count) in rows {
                    li { key: "{index}", class: "chip muted",
                        span { style: "color:{color};", aria_hidden: "true", "● " }
                        "{label} · {count} acct ({share:.0}%)"
                    }
                }
            }
        }
    }
}

#[component]
fn PoolMetadata(pools: Vec<PoolWindowSummary>) -> Element {
    rsx! {
        Table { label: "Pool window metadata", headers: &["Window", "Model", "Weighted left %", "Weighted used %",
            "Remaining / total capacity", "Included / eligible", "Capacity basis", "Estimate", "Next reset (UTC)"],
            rows: pools.iter().map(|pool| {
                let basis = match pool.capacity_basis.as_str() {
                    "equal_entitlements" => "Equal weights",
                    "plan_tier_multiplier" | "plan_multiplier" => "Plan-tier weights",
                    _ => "Provider-defined weights",
                };
                let label = if pool.label.trim().is_empty() { &pool.window_key } else { &pool.label };
                vec![
                    window_label(label).into(), pool.scope_model.clone().unwrap_or_else(|| "—".into()),
                    number(pool.weighted_remaining_percent), number(pool.weighted_used_percent),
                    format_capacity_units(pool.remaining_capacity_units, pool.total_capacity_units).unwrap_or_else(|| "—".into()),
                    format!("{} / {}", pool.included_accounts, pool.eligible_accounts),
                    format!("{basis} ({})", pool.capacity_basis), if pool.is_estimate { "Yes" } else { "No" }.into(),
                    date(pool.next_reset_at),
                ]
            }).collect() }
    }
}

#[component]
fn PastWindows(report: ProviderQuotaReport) -> Element {
    let mask = use_context::<PanelState>().settings.read().mask_emails;
    let names: BTreeMap<_, _> = report.accounts.iter().map(|a| (&a.id, account_name(a, mask))).collect();
    rsx! {
        Table { label: "Past windows", headers: &["Account", "Window", "Model", "Started", "Ended", "Peak used %", "Exhausted", "Early replacement", "Samples"],
            rows: report.past_windows.iter().rev().map(|w| vec![
                names.get(&w.account_id).cloned().unwrap_or_else(|| w.account_id.clone()), window_label(&w.label).to_string(),
                w.scope_model.clone().unwrap_or_else(|| "—".into()), fmt_dt(w.started_at), fmt_dt(w.ended_at),
                number(w.peak_used_percent), date(w.exhausted_at), if w.early_replaced { "Yes" } else { "No" }.into(), w.sample_count.to_string()
            ]).collect() }
    }
}

#[component]
fn RawQuota(report: ProviderQuotaReport, tuning: Tuning) -> Element {
    let mut selected = use_signal(String::new);
    let mut explored = use_signal(|| false);
    let mask = use_context::<PanelState>().settings.read().mask_emails;
    let mut windows = BTreeMap::new();
    for (key, label) in report
        .pools
        .iter()
        .map(|p| (&p.window_key, &p.label))
        .chain(report.accounts.iter().flat_map(|a| &a.windows).map(|w| (&w.window_key, &w.label)))
        .chain(report.samples.iter().map(|sample| (&sample.window_key, &sample.label)))
    {
        windows.entry(key.clone()).or_insert_with(|| window_label(label).to_string());
    }
    let chosen = selected.cloned();
    let Some(window_key) = windows.get_key_value(&chosen).or_else(|| windows.first_key_value()).map(|(key, _)| key.clone()) else {
        return rsx! { p { class: "form-help", "No quota window observed yet." } };
    };
    let samples = report.samples.iter().filter(|s| s.window_key == window_key).cloned().collect();
    let forecasts = report.forecasts.iter().filter(|f| f.account_id.is_some() && f.window_key == window_key).cloned().collect();
    let history = TimeBounds { start: report.as_of - TimeDelta::hours((tuning.range_hours)().into()), end: report.as_of };
    let forecast_until = Some(report.as_of + TimeDelta::hours((tuning.horizon_hours)().into()));
    let names: BTreeMap<_, _> = report.accounts.iter().map(|a| (a.id.clone(), account_name(a, mask))).collect();
    rsx! {
        details { ontoggle: move |_| explored.set(true),
            summary { "Per-account raw series explorer" }
            if explored() {
                Choice { label: "Window", value: window_key.clone(), options: windows.into_iter().collect(), change: move |value| selected.set(value) }
                // This public wrapper renders the explorer and both private table components.
                {rsx! { div { key: "{window_key}",
                    QuotaHistoryChart { samples, forecasts, names,
                        current: report.accounts, window_key,
                        scope: QuotaChartScope { history, forecast_until, history_truncated: report.history_truncated, pool: true }
                    }
                } }}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::providers::dashboard::tests::{fixture, test_panel};

    #[test]
    fn provider_detail_renders_unified_plot_and_collapsed_accordions() {
        fn app() -> Element {
            let panel = test_panel();
            let mut provider = fixture();
            provider.descriptor.capabilities.insert(ProviderCapability::StartRollingWindow);
            let report = ProviderQuotaReport {
                provider: provider.descriptor.clone(),
                accounts: provider.accounts.clone(),
                pools: provider.pools.clone(),
                samples: vec![],
                history_truncated: false,
                past_windows: vec![],
                forecasts: vec![],
                as_of: Utc::now(),
            };
            let settings = panel.settings.cloned();
            let tuning = use_tuning(&settings);
            rsx! {
                DetailContent { provider, stale: false, tuning, report: Some(report), error: None, loading: false }
            }
        }
        let html = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        for text in [
            "← Overview",
            "svg",
            "Forecast & Chart Tuning",
            "Past Windows (0)",
            "Raw Quota Data Tables (0 samples)",
            "Pool Rolling Window Starter",
        ] {
            assert!(html.contains(text), "missing {text}");
        }
        assert!(!html.contains("name: \"open\""), "disclosures must start collapsed");
        assert!(!html.contains("Updating…"));
    }

    #[test]
    fn provider_detail_keeps_chart_mounted_with_updating_badge_while_reloading() {
        fn app() -> Element {
            let panel = test_panel();
            let provider = fixture();
            let report = ProviderQuotaReport {
                provider: provider.descriptor.clone(),
                accounts: provider.accounts.clone(),
                pools: provider.pools.clone(),
                samples: vec![],
                history_truncated: false,
                past_windows: vec![],
                forecasts: vec![],
                as_of: Utc::now(),
            };
            let settings = panel.settings.cloned();
            let tuning = use_tuning(&settings);
            rsx! {
                DetailContent { provider, stale: false, tuning, report: Some(report), error: None, loading: true }
            }
        }
        let html = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(html.contains("svg"), "chart should stay mounted while updating");
        assert!(html.contains("Updating…"), "updating badge should be visible");
    }
}
