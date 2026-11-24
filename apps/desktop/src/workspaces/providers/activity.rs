use std::collections::BTreeMap;

use chrono::{Local, TimeDelta};

use super::{charts::*, dashboard::account_name, data_settings::Archive, routing::MoveConversation, *};

mod detail;
mod evals;

const TABS: [&str; 8] = ["Overview", "Comparisons", "Drift", "Totals", "Thinking", "Requests", "Traces", "Evals"];
const METRICS: &[(&str, &str)] =
    &[("requests", "Requests"), ("tokens", "Tokens"), ("cost", "API-equivalent cost"), ("cache_hit", "Cache hit rate")];
const DEFAULT_RANGE_DAYS: u32 = 1;

fn usd(value: f64) -> String {
    format!("${value:.4}")
}
fn money(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), usd)
}
fn text(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "—".into())
}

#[derive(Clone, PartialEq)]
struct EvalSeed {
    provider: String,
    model: String,
    effort: Option<String>,
    exchange: String,
}

#[component]
pub(super) fn Activity(providers: Vec<ProviderSummary>, mut selected: Signal<String>) -> Element {
    let panel = use_context::<PanelState>();
    let mut tab = use_signal(|| 0usize);
    let board = use_signal(|| None::<Vec<BoardTile>>);
    let mut seed = use_signal(|| None::<EvalSeed>);
    let mut range = use_signal(|| DEFAULT_RANGE_DAYS);
    let mut applied_range = use_signal(|| DEFAULT_RANGE_DAYS);
    let mut show_more = use_signal(|| false);
    let mut filters = use_signal(|| ActivityScope { provider_id: optional(&selected.peek()), ..default_filters() });
    let mut request = use_signal(|| activity_scope(filters.cloned(), DEFAULT_RANGE_DAYS));
    let mut filter_revision = use_signal(|| 0u64);
    use_effect(move || {
        let provider_id = optional(&selected());
        let mut applied = request.peek().clone();
        if applied.provider_id != provider_id {
            applied.provider_id = provider_id.clone();
            applied.account_id = None;
            request.set(applied);
            let mut draft = filters.write();
            draft.provider_id = provider_id;
            draft.account_id = None;
            filter_revision += 1;
        }
    });
    let draft = filters();
    let scope = request();
    let selection = optional(&selected());
    let evals = tab() == 7;
    let active = providers.iter().filter(|p| selection.as_ref().is_none_or(|id| id == &p.descriptor.id));
    let mask = panel.settings.read().mask_emails;
    let mut account_options = vec![(String::new(), "All accounts".into())];
    account_options.extend(active.flat_map(|p| p.accounts.iter().map(|a| (a.id.clone(), account_name(a, mask)))));
    let mut dirty_check = draft.clone();
    dirty_check.from = scope.from;
    dirty_check.tz_offset_mins = scope.tz_offset_mins;
    let changed = dirty_check != scope || applied_range() != range();
    let secondary_count = [&scope.account_id, &scope.client, &scope.model, &scope.effort, &scope.outcome, &scope.search]
        .into_iter()
        .filter(|value| value.is_some())
        .count();
    let on_reset = move |_| {
        let default_scope = default_filters();
        filters.set(default_scope.clone());
        request.set(activity_scope(default_scope, DEFAULT_RANGE_DAYS));
        selected.set(String::new());
        range.set(DEFAULT_RANGE_DAYS);
        applied_range.set(DEFAULT_RANGE_DAYS);
        show_more.set(false);
        filter_revision += 1;
    };
    rsx! {
        section { class: "glass-card stack",
            div { class: "tab-bar", role: "group", aria_label: "Activity views",
                for (i, label) in TABS.into_iter().enumerate() {
                    button { key: "{label}", class: if tab() == i { "tab-btn active" } else { "tab-btn" }, aria_pressed: tab() == i,
                        onclick: move |_| { if i == 7 { seed.set(None); } tab.set(i); }, "{label}" }
                }
            }
            div { class: "row",
                if !evals {
                    Choice { label: "Range", value: range().to_string(), options: pairs(&[("1", "24h"), ("7", "7d"), ("30", "30d"), ("90", "90d")]), change: move |s: String| if let Ok(v) = s.parse() { range.set(v); } }
                    Choice { label: "Source", value: match draft.source { Some(ActivitySource::LocalImport) => "local", Some(ActivitySource::All) => "all", _ => "proxy" },
                        options: pairs(&[("proxy", "Proxy"), ("local", "Local Import"), ("all", "All")]),
                        change: move |s: String| filters.write().source = Some(match s.as_str() { "local" => ActivitySource::LocalImport, "all" => ActivitySource::All, _ => ActivitySource::Proxy }) }
                }
                if !evals {
                    button { class: "btn secondary", aria_expanded: show_more(), aria_controls: "activity-more-filters",
                        onclick: move |_| show_more.toggle(), "More filters ({secondary_count})" }
                }
                button { class: if changed { "btn primary" } else { "btn secondary" }, onclick: move |_| {
                    request.set(activity_scope(filters.cloned(), range()));
                    applied_range.set(range());
                    filter_revision += 1;
                }, if changed { "Apply filters" } else { "Reload activity" } }
            }
            div { id: "activity-more-filters", hidden: evals || !show_more(),
                if !evals && show_more() {
                    div { class: "row",
                        Choice { label: "Account", value: draft.account_id.unwrap_or_default(), options: account_options,
                            change: move |s: String| filters.write().account_id = optional(&s) }
                        Field { label: "Client", value: draft.client.unwrap_or_default(), change: move |s: String| filters.write().client = optional(&s) }
                        Field { label: "Model substring", value: draft.model.unwrap_or_default(), change: move |s: String| filters.write().model = optional(&s) }
                        Field { label: "Effort", value: draft.effort.unwrap_or_default(), change: move |s: String| filters.write().effort = optional(&s) }
                        Choice { label: "Outcome", value: draft.outcome.unwrap_or_default(), options: pairs(&[("", "All"), ("failed", "Failed")]),
                            change: move |s: String| filters.write().outcome = optional(&s) }
                        Field { label: "Search", kind: "search", value: draft.search.unwrap_or_default(), change: move |s: String| filters.write().search = optional(&s) }
                    }
                }
            }
            if evals { p { class: "form-help", "Eval cases are saved separately from the activity time range." } }
        }
        {rsx! { div { key: "{filter_revision}", class: "stack",
            if scope.provider_id == selection {
                match tab() {
                    0 => rsx! { Overview { scope, board, on_reset, on_compare: move |_| tab.set(1) } },
                    1 => rsx! { Compare { scope } },
                    2 => rsx! { Drift { scope, providers } },
                    3 => rsx! { Totals { scope } },
                    4 => rsx! { Thinking { scope, providers } },
                    5 => rsx! { detail::Requests { scope, providers, on_reset } },
                    6 => rsx! { detail::Traces { scope, on_reset, pin: move |value| { seed.set(Some(value)); tab.set(7); } } },
                    _ => rsx! { evals::Evals { providers, scoped_provider: scope.provider_id, seed: seed() } },
                }
            }
        } }}
    }
}

fn default_filters() -> ActivityScope {
    ActivityScope { source: Some(ActivitySource::Proxy), ..Default::default() }
}

fn activity_scope(mut scope: ActivityScope, days: u32) -> ActivityScope {
    scope.from = Some(Utc::now() - TimeDelta::days(days.into()));
    scope.tz_offset_mins = Some(Local::now().offset().local_minus_utc() / 60);
    scope
}

#[component]
fn ActivityEmpty(message: &'static str, #[props(optional)] hint: Option<&'static str>, on_reset: EventHandler<()>) -> Element {
    rsx! {
        div { class: "empty-hero", style: "max-width: none;",
            p { role: "status", strong { "{message}" } }
            if let Some(hint) = hint { p { class: "form-help", "{hint}" } }
            button { class: "btn primary empty-actions", onclick: move |_| on_reset.call(()), "Reset filters" }
        }
    }
}

#[component]
fn Overview(
    scope: ActivityScope,
    board: Signal<Option<Vec<BoardTile>>>,
    on_reset: EventHandler<()>,
    on_compare: EventHandler<()>,
) -> Element {
    let client = use_context::<HubClient>();
    let history_client = client.clone();
    let panel = use_context::<PanelState>();
    let data = use_resource(move || {
        let _ = (panel.epoch)();
        let (client, scope) = (client.clone(), scope.clone());
        async move { timed(client.activity_overview(&scope)).await }
    });
    let history = use_resource(move || {
        let empty = data.read().as_ref().is_some_and(|result| result.as_ref().is_ok_and(|report| report.requests == 0));
        let client = history_client.clone();
        async move {
            if !empty {
                return None;
            }
            let probe = ActivityScope { from: Some(DateTime::UNIX_EPOCH), limit: Some(1), ..Default::default() };
            timed(client.activity_requests(&probe)).await.ok().map(|page| !page.items.is_empty())
        }
    });
    let has_history = history.cloned().flatten();
    let result = data.read();
    read_view(result.as_ref(), |report| rsx! { OverviewBoard { report: report.clone(), board, has_history, on_reset, on_compare } })
}

#[component]
fn OverviewBoard(
    report: ActivityOverview,
    board: Signal<Option<Vec<BoardTile>>>,
    has_history: Option<bool>,
    on_reset: EventHandler<()>,
    on_compare: EventHandler<()>,
) -> Element {
    let mut configure = use_signal(|| false);
    if report.requests == 0 {
        let (message, hint) = if has_history == Some(false) {
            ("No activity recorded yet", "New requests will appear here when the hub records activity.")
        } else {
            ("No requests matched the current filters", "Reset the filters, choose a longer range, or select All sources.")
        };
        return rsx! { ActivityEmpty { message, hint, on_reset } };
    }
    let custom_tiles = board.cloned();
    let default_layout = custom_tiles.is_none();
    let rows = custom_tiles.unwrap_or_else(|| {
        let mut tiles = report.board_tiles.clone();
        if !tiles.iter().any(|t| t.kind == "model_effort") {
            tiles.push(BoardTile { id: "model_effort".into(), kind: "model_effort".into(), metric: "cost".into(), span: 1, visible: true });
        }
        tiles
    });
    let unpriced = report.unpriced_models.join(", ");
    let hit_rate = effective_cache_hit_rate(report.input_tokens, report.cache_read_tokens, report.cache_hit_rate);
    let hit_pct = fmt_hit_pct(hit_rate);
    rsx! {
        div { class: "row",
            h2 { "Activity overview" }
            button { class: "btn secondary", aria_expanded: configure(), aria_controls: "activity-tile-options",
                onclick: move |_| configure.toggle(), "Configure overview tiles" }
        }
        div { id: "activity-tile-options", hidden: !configure(),
            if configure() {
                div { class: "provider-inset",
                    for (i, tile) in rows.iter().enumerate() {
                        {
                            let visible_rows = rows.clone();
                            let span_rows = rows.clone();
                            let metric_rows = rows.clone();
                            rsx! {
                                div { key: "{tile.id}", class: "row quota-window",
                                    strong { "{tile_name(&tile.kind)}" }
                                    Check { label: "Visible", value: tile.visible, change: move |v| edit_tile(board, &visible_rows, i, |tile| tile.visible = v) }
                                    Choice { label: "Columns", value: tile.span.to_string(), options: pairs(&[("1", "1"), ("2", "2"), ("3", "3")]),
                                        change: move |v: String| if let Ok(span) = v.parse() { edit_tile(board, &span_rows, i, |tile| tile.span = span); } }
                                    if matches!(tile.kind.as_str(), "trend" | "heatmap" | "rhythm") {
                                        Choice { label: "Metric", value: tile.metric.clone(), options: pairs(METRICS),
                                            change: move |v| edit_tile(board, &metric_rows, i, |tile| tile.metric = v) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        div { class: "compact-kpi-grid", style: "grid-template-columns: repeat(auto-fit, minmax(min(100%, 9rem), 1fr));",
            article { class: "glass-card stat-card", title: unpriced.clone(),
                span { class: "label-tech", "API-equivalent cost" }
                strong { class: "value", "{usd(report.api_equivalent_cost_usd)}" }
                span { "List-price estimate, not billed spend" }
                span { "{fmt_count(report.unpriced_requests)} unpriced requests" }
                if !unpriced.is_empty() { details { summary { "Unpriced models" } p { "{unpriced}" } } }
            }
            article { class: "glass-card stat-card", span { class: "label-tech", "Tokens" }
                strong { class: "value", "{fmt_count(report.total_tokens)}" } span { "{fmt_count(report.output_tokens)} output" }
            }
            article { class: "glass-card stat-card", span { class: "label-tech", "Requests" }
                strong { class: "value", "{fmt_count(report.requests)}" } span { "{fmt_count(report.failures)} failed" }
            }
            article { class: "glass-card stat-card", span { class: "label-tech", "Cache hit" }
                strong { class: "value", "{hit_pct}" }
                span { "{fmt_count(report.cache_read_tokens)} / {fmt_count(report.input_tokens)} input" }
                span { "{usd(report.cache_savings_usd)} estimated savings" }
            }
        }
        div { class: "activity-board", "data-default-layout": if default_layout { "true" } else { "false" },
            for tile in rows.iter().filter(|tile| tile.visible) {
                article { key: "{tile.id}", class: "glass-card stack activity-span-{tile.span.clamp(1, 3)}", "data-tile": "{tile.kind}",
                    h2 { "{tile_name(&tile.kind)}" }
                    {
                        let metric = Metric::parse(&tile.metric);
                        match tile.kind.as_str() {
                            "trend" => rsx! { ActivityTrendChart { points: report.trend.clone(), metric } },
                            "heatmap" => rsx! { ActivityHeatmap { cells: report.heatmap.clone(), metric } },
                            "rhythm" => rsx! { ActivityRhythmChart { points: report.rhythm.clone(), metric } },
                            "cache" if report.trend.is_empty() || hit_rate.is_none() => rsx! { p { class: "form-help", "{NO_RANGE_DATA}" } },
                            "cache" => rsx! {
                                div { class: "cache-summary stack",
                                    div { class: "spread",
                                        strong { class: "mono", "{hit_pct} cache hit rate" }
                                        span { class: "muted", "{usd(report.cache_savings_usd)} estimated savings" }
                                    }
                                    svg { class: "quota-meter cache-meter", view_box: "0 0 100 6", preserve_aspect_ratio: "none", "aria-hidden": "true",
                                        rect { width: "100", height: "6", rx: "3", class: "quota-track" }
                                        if let Some(rate) = hit_rate {
                                            rect { width: "{rate * 100.0:.2}", height: "6", rx: "3", class: "cache-meter-fill" }
                                        }
                                    }
                                    p { class: "muted",
                                        "{fmt_count(report.cache_read_tokens)} read · {fmt_count(report.cache_write_tokens)} write · {fmt_count(report.input_tokens.saturating_sub(report.cache_read_tokens))} uncached input"
                                    }
                                    CacheHitRateChart { points: report.trend.clone() }
                                }
                            },
                            "models" => breakdown("Models", &report.by_model),
                            "accounts" => breakdown("Accounts", &report.by_account),
                            "clients" => breakdown("Clients", &report.by_client),
                            "model_effort" => breakdown("Model / Effort", &report.by_model_effort),
                            "compare" => rsx! { button { class: "btn btn-glass", onclick: move |_| on_compare.call(()), "Open model / effort comparison" } },
                            _ => rsx! { p { class: "muted", "This tile is not supported by this desktop build." } },
                        }
                    }
                }
            }
        }
        details { summary { "Traffic coverage, pricing and retention" }
            p { class: "form-help", "{fmt_count(report.matched_proxy_requests)} matched proxy · {fmt_count(report.bypassed_local_requests)} bypassed local" }
            p { class: "form-help", "Price catalog as of {report.price_as_of} · {report.cache_write_assumption} · detail retention {report.detail_retention_days} days." }
        }
    }
}

fn edit_tile(mut board: Signal<Option<Vec<BoardTile>>>, defaults: &[BoardTile], index: usize, edit: impl FnOnce(&mut BoardTile)) {
    if let Some(tile) = board.write().get_or_insert_with(|| defaults.to_vec()).get_mut(index) {
        edit(tile);
    }
}

fn tile_name(kind: &str) -> &str {
    match kind {
        "trend" => "Trend",
        "cache" => "Cache",
        "heatmap" => "Heatmap",
        "rhythm" => "Rhythm",
        "models" => "Models",
        "accounts" => "Accounts",
        "clients" => "Clients",
        "model_effort" => "Model / Effort",
        "compare" => "Compare",
        _ => kind,
    }
}

fn breakdown(label: &'static str, list: &[BreakdownRow]) -> Element {
    let mask = try_consume_context::<PanelState>().is_some_and(|p| p.settings.read().mask_emails);
    rsx! { Table { label, headers: &["Group", "Requests", "Failed", "Tokens", "Output", "Cache read", "Cache hit %", "Thinking", "API-equivalent USD", "Unpriced"],
    rows: list.iter().map(|r| {
        let input = if r.input_tokens > 0 { r.input_tokens } else { r.total_tokens.saturating_sub(r.output_tokens) };
        vec![
            if label == "Accounts" && mask && r.label.contains('@') { mask_email(&r.label) } else { r.label.clone() },
            fmt_count(r.requests), fmt_count(r.failures), fmt_count(r.total_tokens), fmt_count(r.output_tokens),
            fmt_count(r.cache_read_tokens), fmt_hit_pct(effective_cache_hit_rate(input, r.cache_read_tokens, r.cache_hit_rate)),
            fmt_count(r.thinking_tokens), usd(r.api_equivalent_cost_usd), fmt_count(r.unpriced_requests)
        ]
    }).collect() } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_overview_shows_recovery_instead_of_kpis_and_tiles() {
        fn app() -> Element {
            let board = use_signal(|| None);
            let report: ActivityOverview = serde_json::from_value(serde_json::json!({
                "api_equivalent_cost_usd": 0.0, "unpriced_requests": 0, "unpriced_models": [],
                "price_as_of": "", "cache_write_assumption": "", "total_tokens": 0,
                "input_tokens": 0, "output_tokens": 0, "cache_read_tokens": 0, "cache_write_tokens": 0,
                "thinking_tokens": 0, "requests": 0, "failures": 0, "cache_hit_rate": 0.0,
                "cache_savings_usd": 0.0, "matched_proxy_requests": 0, "bypassed_local_requests": 0,
                "trend": [], "heatmap": [], "rhythm": [], "by_model": [], "by_account": [],
                "by_client": [], "by_model_effort": [], "detail_retention_days": 7,
                "board_tiles": [
                    {"id": "trend", "kind": "trend", "metric": "requests", "span": 1, "visible": true}
                ]
            }))
            .unwrap();
            rsx! { OverviewBoard { report, board, has_history: Some(true), on_reset: |_| {}, on_compare: |_| {} } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("No requests matched the current filters"));
        assert!(rendered.contains("Reset filters"));
        assert!(!rendered.contains("compact-kpi-grid"));
        assert!(!rendered.contains("activity-board"));
    }

    #[test]
    fn legacy_breakdown_uses_total_minus_output_for_cache_hit_rate() {
        fn app() -> Element {
            let row = serde_json::from_value(serde_json::json!({
                "key": "legacy", "label": "Legacy", "requests": 1, "failures": 0,
                "total_tokens": 120, "output_tokens": 20, "cache_read_tokens": 80,
                "thinking_tokens": 0, "api_equivalent_cost_usd": 0.0, "unpriced_requests": 0
            }))
            .unwrap();
            breakdown("Models", &[row])
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("80.0%"));
    }
}

#[component]
fn Compare(scope: ActivityScope) -> Element {
    let client = use_context::<HubClient>();
    let mut baseline = use_signal(String::new);
    let data = use_resource(move || {
        let client = client.clone();
        let scope = ActivityScope { baseline_key: optional(&baseline()), ..scope.clone() };
        async move { timed_for(Duration::from_secs(25), client.activity_compare(&scope)).await }
    });
    let result = data.read();
    rsx! { section { class: "glass-card stack",
        h2 { "Model / effort comparison" }
        p { class: "muted", "API list-price estimates, not billing. Task and retry metrics depend on retained detail." }
        {read_view(result.as_ref(), |report| {
            let mut options = vec![(String::new(), "Automatic".into())];
            options.extend(report.rows.iter().map(|r| (r.key.clone(), r.key.clone())));
            rsx! {
                Choice { label: "Baseline", value: baseline(), options, change: move |v| baseline.set(v) }
                p { "Effective baseline: {text(&report.baseline_key)} · pricing as of {report.price_as_of}" }
                Table { label: "Model and effort comparison", headers: &[
                    "Model", "Effort", "$/1M input", "$/1M output", "$/1M cache read", "$/1M cache write (5m)", "Cost USD", "Requests", "Failed",
                    "Mean cost/req", "Cost/1M tokens", "Cache hit %", "Failed cost %", "Thinking/output %", "Fast/flex %", "Root tasks",
                    "Median task $", "Mean task $", "P90 task $", "Median tokens/task", "Median req/task", "Mixed tasks %", "Other models %", "Subagent $",
                    "Baseline cost ×", "Baseline req cost ×", "Baseline cache Δ pp", "Detail coverage %"
                ], rows: report.rows.iter().map(|r| vec![
                    r.model.clone(), text(&r.effort), number(r.price.as_ref().map(|p| p.input_per_m)), number(r.price.as_ref().map(|p| p.output_per_m)),
                    number(r.price.as_ref().map(|p| p.cache_read_per_m)), number(r.price.as_ref().map(|p| p.cache_write_5m_per_m)),
                    usd(r.api_equivalent_cost_usd), r.requests.to_string(), r.failures.to_string(), money(r.mean_cost_per_request), money(r.cost_per_m_tokens),
                    number(Some(r.cache_hit_pct)), number(Some(r.failed_cost_share_pct)), number(Some(r.thinking_output_share_pct)), number(Some(r.fast_flex_share_pct)),
                    r.root_tasks.to_string(), money(r.median_task_cost), money(r.mean_task_cost), money(r.p90_task_cost),
                    r.median_tokens_per_task.map_or_else(|| "—".into(), |n| n.to_string()), r.median_requests_per_task.map_or_else(|| "—".into(), |n| n.to_string()),
                    number(Some(r.mixed_task_share_pct)), number(Some(r.other_model_share_pct)), usd(r.subagent_cost_usd),
                    number(r.baseline_cost_multiple), number(r.baseline_req_cost_multiple), number(r.baseline_cache_pp_delta), number(Some(r.detail_coverage_pct))
                ]).collect() }
                details { summary { "Reference price catalog ({report.price_catalog.len()})" }
                    Table { label: "Reference price catalog", headers: &["Model", "Pattern", "Effective date", "Currency", "Input / 1M", "Output / 1M", "Cache read / 1M", "Cache write 5m / 1M", "Source"],
                        rows: report.price_catalog.iter().map(|p| vec![
                            p.display_name.clone(), p.model_pattern.clone(), p.effective_from.clone(), p.currency.clone(),
                            number(Some(p.input_per_m)), number(Some(p.output_per_m)), number(Some(p.cache_read_per_m)), number(Some(p.cache_write_5m_per_m)), p.source_note.clone()
                        ]).collect() }
                }
            }
        })}
    } }
}

#[component]
fn Drift(scope: ActivityScope, providers: Vec<ProviderSummary>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let query = scope.clone();
    let data = use_resource(move || {
        let (client, scope) = (client.clone(), query.clone());
        let _ = (panel.epoch)();
        async move { timed_for(Duration::from_secs(25), client.activity_drift(&scope)).await }
    });
    let result = data.read();
    let allowed = probe_providers(&providers, &scope, ProviderCapability::RunCanary);
    rsx! { section { class: "glass-card stack",
        h2 { "Model / effort drift" }
        p { class: "muted", "Judgments require ≥30 replies/day and ≥5 valid prior days in a 14-day reference window. Client changes are context, not proof of cause." }
        {read_view(result.as_ref(), |report| rsx! {
            if report.series.is_empty() { p { class: "muted", "No drift series in this scope." } }
            for s in &report.series {
                article { key: "{s.model}-{s.effort.as_deref().unwrap_or_default()}", class: "provider-inset stack",
                    h3 { "{s.model} · {text(&s.effort)}" }
                    p { "{s.summary} · {s.outlier_days} outlier days" }
                    DriftBandChart { days: s.days.clone(), markers: report.markers.clone() }
                    details { summary { "Daily drift measurements" }
                        Table { label: "Daily drift", headers: &["Date", "Replies", "Thinking/reply", "Output/reply", "No thinking %", "Median TTFT (ms)", "Failure %", "Normal low", "Normal median", "Normal high", "Judgment"],
                            rows: s.days.iter().map(|d| vec![ d.date.clone(), d.replies.to_string(), number(Some(d.thinking_per_reply)), number(Some(d.output_per_reply)),
                                number(Some(d.no_thinking_share * 100.0)), number(d.median_ttft_ms), number(Some(d.failure_rate * 100.0)), number(d.baseline_low),
                                number(d.baseline_median_thinking), number(d.baseline_high),
                                if d.outlier { "Outlier" } else if d.judgeable { "Within band" } else { "Insufficient coverage" }.into()
                            ]).collect() }
                    }
                }
            }
            Table { label: "Client / request changes", headers: &["Date", "Kind", "Change"],
                rows: report.markers.iter().map(|m| vec![m.date.clone(), m.kind.clone(), m.label.clone()]).collect() }
            if !allowed.is_empty() { CanaryEditor { key: "{panel.epoch}", providers: allowed.clone(), schedule: report.schedule.clone() } }
            h3 { "Recent canary runs" }
            Table { label: "Canary runs", headers: &["Ran", "Provider", "Model", "Effort", "Passed / run", "Mean thinking", "Mean TTFT (ms)", "Error"],
                rows: report.recent_canaries.iter().map(|r| vec![fmt_dt(r.ran_at), r.provider_id.clone(), r.model_id.clone(), text(&r.effort),
                    format!("{}/{}", r.prompts_passed, r.prompts_run), number(Some(r.mean_thinking_tokens)), number(r.mean_ttft_ms), text(&r.error)
                ]).collect() }
        })}
    } }
}

fn probe_providers(providers: &[ProviderSummary], scope: &ActivityScope, cap: ProviderCapability) -> Vec<ProviderSummary> {
    providers
        .iter()
        .filter(|p| scope.provider_id.as_ref().is_none_or(|id| id == &p.descriptor.id) && supports(p, None, cap))
        .cloned()
        .collect()
}

fn provider_choices(providers: &[ProviderSummary]) -> Vec<(String, String)> {
    providers.iter().map(|p| (p.descriptor.id.clone(), p.descriptor.label.clone())).collect()
}

#[component]
fn CanaryEditor(providers: Vec<ProviderSummary>, schedule: CanarySchedule) -> Element {
    let mut id = use_signal(|| providers.first().map(|p| p.descriptor.id.clone()).unwrap_or_default());
    let mut model = use_signal(String::new);
    let mut effort = use_signal(String::new);
    let mut enabled = use_signal(|| schedule.enabled);
    let mut interval = use_signal(|| if [6, 12, 24].contains(&schedule.interval_hours) { schedule.interval_hours } else { 12 });
    let mut targets = use_signal(|| schedule.targets.clone());
    let target = optional(&model()).map(|model_id| CanaryTarget { provider_id: id(), model_id, effort: optional(&effort()) });
    let provider = providers.iter().find(|p| p.descriptor.id == id()).cloned();
    rsx! { article { class: "provider-inset stack",
        h3 { "Canary schedule" }
        p { class: "muted", "Scheduled and on-demand canaries send real requests and can consume quota." }
        div { class: "row",
            Choice { label: "Provider", value: id(), options: provider_choices(&providers), change: move |v| id.set(v) }
            Field { label: "Model ID", value: model(), change: move |v| model.set(v) }
            Field { label: "Effort (optional)", value: effort(), change: move |v| effort.set(v) }
            button { class: "btn btn-glass", disabled: target.is_none(), onclick: {
                let target = target.clone();
                move |_| if let Some(target) = &target && !targets.peek().contains(target) { targets.write().push(target.clone()); }
            }, "Add scheduled target" }
            if let Some(provider) = provider.clone() {
                OperationButton { provider, label: "Run Canary", confirm: true, disabled: target.is_none(),
                    command: ProviderCommand::Probe(ProbeCommand::RunCanary { model_id: model().trim().into(), effort: optional(&effort()), account_id: None }) }
            }
        }
        for (i, target) in targets().iter().enumerate() {
            div { class: "row", span { "{target.provider_id} · {target.model_id} · {text(&target.effort)}" }
                button { class: "btn btn-glass", onclick: move |_| { targets.write().remove(i); }, "Remove target" }
            }
        }
        div { class: "row",
            Check { label: "Enable schedule", value: enabled(), change: move |v| enabled.set(v) }
            Choice { label: "Interval", value: interval().to_string(), options: pairs(&[("6", "6h"), ("12", "12h"), ("24", "24h")]),
                change: move |v: String| if let Ok(n) = v.parse() { interval.set(n); } }
            if let Some(provider) = provider {
                OperationButton { provider, label: "Save schedule", confirm: true, disabled: enabled() && targets.read().is_empty(),
                    command: ProviderCommand::Probe(ProbeCommand::ConfigureCanarySchedule { schedule: CanarySchedule { enabled: enabled(), interval_hours: interval(), targets: targets() } }) }
            }
        }
    } }
}

const GRANULARITIES: &[(TotalsGranularity, &str)] = &[
    (TotalsGranularity::Hour, "Hour"),
    (TotalsGranularity::Day, "Day"),
    (TotalsGranularity::Week, "Week"),
    (TotalsGranularity::Month, "Month"),
];

#[component]
fn Totals(scope: ActivityScope) -> Element {
    let client = use_context::<HubClient>();
    let mut granularity = use_signal(|| TotalsGranularity::Day);
    let data = use_resource(move || {
        let (client, scope, g) = (client.clone(), scope.clone(), granularity());
        async move { timed(client.activity_totals(g, &scope)).await }
    });
    let result = data.read();
    let row = |b: &TotalsBucket, period: String| {
        vec![
            period,
            fmt_count(b.requests),
            fmt_count(b.failures),
            fmt_count(b.input_tokens),
            fmt_count(b.output_tokens),
            fmt_count(b.cache_read_tokens),
            fmt_count(b.cache_write_tokens),
            fmt_hit_pct(effective_cache_hit_rate(b.input_tokens, b.cache_read_tokens, b.cache_hit_rate)),
            fmt_count(b.thinking_tokens),
            usd(b.api_equivalent_cost_usd),
            fmt_count(b.unpriced_requests),
            if b.includes_retained_aggregates { "Retained aggregates" } else { "Detail" }.into(),
        ]
    };
    rsx! { section { class: "glass-card stack",
        h2 { "Activity totals" }
        Choice { label: "Granularity", value: format!("{:?}", granularity()),
            options: GRANULARITIES.iter().map(|(value, label)| (format!("{value:?}"), label.to_string())).collect(),
            change: move |key: String| if let Some((v, _)) = GRANULARITIES.iter().find(|(v, _)| format!("{v:?}") == key) { granularity.set(*v); } }
        p { class: "muted", "Historical totals can survive request-detail expiry. API-equivalent USD is not billing. Local hourly views use the displayed session’s local UTC offset." }
        {read_view(result.as_ref(), |r| rsx! {
            p { "Showing {granularity_label(r.granularity)} aggregates" }
            Table { label: "Activity totals", headers: &["Period", "Requests", "Failures", "Input", "Output", "Cache read", "Cache write", "Cache hit %", "Thinking", "API-equivalent USD", "Unpriced", "Coverage"],
                rows: r.buckets.iter().map(|b| row(b, b.period.clone())).chain(std::iter::once(row(&r.grand_total, "Grand Total".into()))).collect() }
        })}
    } }
}

#[component]
fn Thinking(scope: ActivityScope, providers: Vec<ProviderSummary>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let query = scope.clone();
    let data = use_resource(move || {
        let (client, scope) = (client.clone(), query.clone());
        let _ = (panel.epoch)();
        async move { timed_for(Duration::from_secs(25), client.activity_thinking(&scope)).await }
    });
    let result = data.read();
    let allowed = probe_providers(&providers, &scope, ProviderCapability::RunThinkingTest);
    rsx! { section { class: "glass-card stack",
        h2 { "Thinking analysis" }
        {read_view(result.as_ref(), |r| rsx! {
            p { class: "gateprompt", role: "status", "{r.verdict}" }
            p { "{fmt_count(r.total_turns)} turns · {fmt_count(r.thinking_tokens)} thinking tokens · {usd(r.thinking_cost_usd)} estimated thinking cost" }
            p { class: "muted", "{r.method_note}" }
            {thinking_buckets("Thinking buckets", &r.buckets)}
            {thinking_buckets("Effort kinds", &r.kinds)}
            Table { label: "Savings alternatives (estimates)", headers: &["Alternative", "Target model", "Effort", "Estimated cost", "Estimated savings", "Estimated savings %"],
                rows: r.alternatives.iter().map(|a| vec![format!("Estimate · {}", a.label), a.target_model.clone(), text(&a.target_effort),
                    usd(a.estimated_cost_usd), usd(a.estimated_savings_usd), number(Some(a.estimated_savings_pct))]).collect() }
            if !allowed.is_empty() { ThinkingProbe { providers: allowed.clone(), model_default: r.selected_model.clone(), effort_default: r.selected_effort.clone() } }
            h3 { "Executed Thinking Tests" }
            Table { label: "Thinking test runs", headers: &["Ran", "Baseline", "Effort", "Comparison", "Compare effort", "Turns", "Judge", "Equivalence", "Measured cost Δ %", "Status"],
                rows: r.test_runs.iter().map(|t| vec![fmt_dt(t.ran_at), t.baseline_model.clone(), t.baseline_effort.clone(), t.compare_model.clone(), text(&t.compare_effort),
                    t.sampled_turns.to_string(), text(&t.judge_model), number(t.equivalence_rate), number(t.measured_cost_delta_pct), t.status.clone()]).collect() }
        })}
    } }
}

fn thinking_buckets(label: &'static str, rows: &[ThinkingBucket]) -> Element {
    rsx! { Table { label, headers: &["Bucket", "Turns", "Thinking", "Output", "Estimated thinking cost"],
    rows: rows.iter().map(|b| vec![b.label.clone(), fmt_count(b.turns), fmt_count(b.thinking_tokens), fmt_count(b.output_tokens), usd(b.estimated_thinking_cost_usd)]).collect() } }
}

#[component]
fn ThinkingProbe(providers: Vec<ProviderSummary>, model_default: Option<String>, effort_default: Option<String>) -> Element {
    let mut id = use_signal(|| providers.first().map(|p| p.descriptor.id.clone()).unwrap_or_default());
    let mut model = use_signal(|| model_default.unwrap_or_default());
    let mut effort = use_signal(|| effort_default.unwrap_or_default());
    let mut compare = use_signal(String::new);
    let mut compare_effort = use_signal(String::new);
    let mut count = use_signal(|| String::from("6"));
    let parsed = count().parse::<u32>().ok().filter(|n| (1..=100).contains(n));
    rsx! { article { class: "provider-inset stack",
        h3 { "Thinking test" }
        p { class: "muted", "An explicit paid/quota-consuming probe; judge scores are not ground truth." }
        div { class: "row",
            Choice { label: "Provider", value: id(), options: provider_choices(&providers), change: move |v| id.set(v) }
            Field { label: "Baseline model", value: model(), change: move |v| model.set(v) }
            Field { label: "Baseline effort", value: effort(), change: move |v| effort.set(v) }
            Field { label: "Compare model", value: compare(), change: move |v| compare.set(v) }
            Field { label: "Compare effort", value: compare_effort(), change: move |v| compare_effort.set(v) }
            Field { label: "Sample count (1–100)", kind: "number", value: count(), change: move |v| count.set(v) }
            if let Some(provider) = providers.iter().find(|p| p.descriptor.id == id()) {
                OperationButton { provider: provider.clone(), label: "Run Thinking Test", confirm: true, disabled: parsed.is_none() || optional(&model()).is_none() || optional(&effort()).is_none(),
                    command: ProviderCommand::Probe(ProbeCommand::RunThinkingTest { model_id: model().trim().into(), effort: effort().trim().into(),
                        compare_model_id: optional(&compare()), compare_effort: optional(&compare_effort()), sample_count: parsed.unwrap_or_default() }) }
            }
        }
    } }
}
