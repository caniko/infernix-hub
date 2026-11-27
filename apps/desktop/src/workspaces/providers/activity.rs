use std::collections::BTreeMap;

use chrono::{Local, TimeDelta};

use super::{charts::*, dashboard::account_name, data_settings::Archive, routing::MoveConversation, *};

mod detail;
mod evals;

const TABS: [(&str, &str); 9] = [
    ("overview", "Overview"),
    ("breakdowns", "Breakdowns"),
    ("comparisons", "Comparisons"),
    ("drift", "Drift"),
    ("totals", "Totals"),
    ("thinking", "Thinking"),
    ("requests", "Requests"),
    ("traces", "Traces"),
    ("evals", "Evals"),
];
const BREAKDOWNS_TAB: usize = 1;
const EVALS_TAB: usize = TABS.len() - 1;
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
    let mut activity_subtab = panel.activity_subtab;
    let initial_tab = activity_subtab.peek().as_deref().and_then(|target| TABS.iter().position(|(key, _)| *key == target)).unwrap_or(0);
    let mut tab = use_signal(|| initial_tab);
    let board = use_signal(|| None::<Vec<BoardTile>>);
    let mut seed = use_signal(|| None::<EvalSeed>);
    let mut range = use_signal(|| DEFAULT_RANGE_DAYS);
    let mut applied_range = use_signal(|| DEFAULT_RANGE_DAYS);
    let mut show_more = use_signal(|| false);
    let mut filters = use_signal(|| ActivityScope { provider_id: optional(&selected.peek()), ..default_filters() });
    let mut request = use_signal(|| activity_scope(filters.cloned(), DEFAULT_RANGE_DAYS));
    let mut filter_revision = use_signal(|| 0u64);
    use_effect(move || {
        if let Some(target) = activity_subtab() {
            if let Some(index) = TABS.iter().position(|(key, _)| *key == target) {
                if index == EVALS_TAB {
                    seed.set(None);
                }
                if *tab.peek() != index {
                    tab.set(index);
                }
            }
            activity_subtab.set(None);
        }
    });
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
    let evals = tab() == EVALS_TAB;
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
            div { class: "segmented activity-subtabs", role: "group", aria_label: "Activity views",
                for (i, (_, label)) in TABS.into_iter().enumerate() {
                    button { key: "{label}", class: "seg-btn", aria_pressed: tab() == i,
                        onclick: move |_| { if i == EVALS_TAB { seed.set(None); } tab.set(i); }, "{label}" }
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
                match TABS[tab()].0 {
                    "overview" => rsx! { Overview { scope, board, on_reset, on_breakdowns: move |_| tab.set(BREAKDOWNS_TAB) } },
                    "breakdowns" => rsx! { Breakdowns { scope } },
                    "comparisons" => rsx! { Compare { scope } },
                    "drift" => rsx! { Drift { scope, providers } },
                    "totals" => rsx! { Totals { scope } },
                    "thinking" => rsx! { Thinking { scope, providers } },
                    "requests" => rsx! { detail::Requests { scope, providers, on_reset } },
                    "traces" => rsx! { detail::Traces { scope, on_reset, pin: move |value| { seed.set(Some(value)); tab.set(EVALS_TAB); } } },
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
    on_breakdowns: EventHandler<()>,
) -> Element {
    let history_client = use_context::<HubClient>();
    let data = use_activity_overview(scope);
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
    read_view(result.as_ref(), |report| rsx! { OverviewBoard { report: report.clone(), board, has_history, on_reset, on_breakdowns } })
}

fn use_activity_overview(scope: ActivityScope) -> Resource<Result<ActivityOverview, UiError>> {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    use_resource(move || {
        let _ = (panel.epoch)();
        let (client, scope) = (client.clone(), scope.clone());
        async move { timed(client.activity_overview(&scope)).await }
    })
}

#[component]
fn OverviewBoard(
    report: ActivityOverview,
    board: Signal<Option<Vec<BoardTile>>>,
    has_history: Option<bool>,
    on_reset: EventHandler<()>,
    on_breakdowns: EventHandler<()>,
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
    let rows: Vec<_> = custom_tiles
        .unwrap_or_else(|| report.board_tiles.clone())
        .into_iter()
        .filter(|tile| matches!(tile.kind.as_str(), "trend" | "cache" | "heatmap" | "rhythm"))
        .collect();
    let unpriced = report.unpriced_models.join(", ");
    let hit_pct = fmt_hit_pct(effective_cache_hit_rate(report.input_tokens, report.cache_read_tokens, report.cache_hit_rate));
    let visible = rows.iter().filter(|tile| tile.visible);
    let featured = default_layout && visible.clone().take(4).map(|tile| tile.kind.as_str()).eq(["trend", "cache", "heatmap", "rhythm"]);
    let mut tiles = visible.map(|tile| {
        let metric = Metric::parse(&tile.metric);
        rsx! {
            article { key: "{tile.id}", class: "glass-card stack activity-span-{tile.span.clamp(1, 3)}", "data-tile": "{tile.kind}",
                div { class: "row activity-tile-header",
                    h2 { "{tile_name(&tile.kind)}" }
                    if matches!(tile.kind.as_str(), "trend" | "cache")
                        && let Some(label) = bucket_badge(report.bucket_seconds) {
                        Chip { label: label.to_string(), tone: Some("amber".to_string()) }
                    }
                }
                {match tile.kind.as_str() {
                    "trend" => rsx! { ActivityTrendChart { points: report.trend.clone(), metric, bucket_seconds: report.bucket_seconds } },
                    "heatmap" => rsx! { ActivityHeatmap { cells: report.heatmap.clone(), metric, covered_weekdays: report.covered_weekdays.clone() } },
                    "rhythm" => rsx! { ActivityRhythmChart { points: report.rhythm.clone(), metric } },
                    "cache" => rsx! { CacheHitRateChart { points: report.trend.clone(), bucket_seconds: report.bucket_seconds } },
                    _ => rsx! {},
                }}
            }
        }
    });
    let featured_tiles: Vec<_> = tiles.by_ref().take(if featured { 4 } else { 0 }).collect();
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
        div { class: "compact-kpi-grid activity-kpis activity-kpi-grid",
            article { class: "glass-card stat-card",
                div { class: "row",
                    span { class: "label-tech", "API-equivalent value" }
                    strong { class: "value", "{usd(report.api_equivalent_cost_usd)}" }
                }
                span { "{fmt_count(report.unpriced_requests)} unpriced requests" }
            }
            article { class: "glass-card stat-card",
                div { class: "row",
                    span { class: "label-tech", "Total tokens" }
                    strong { class: "value", "{fmt_count(report.total_tokens)}" }
                }
                span { "{fmt_count(report.output_tokens)} output" }
            }
            article { class: "glass-card stat-card",
                div { class: "row",
                    span { class: "label-tech", "Requests" }
                    strong { class: "value", "{fmt_count(report.requests)}" }
                }
                span { "{fmt_count(report.failures)} failed" }
            }
            article { class: "glass-card stat-card",
                div { class: "row",
                    span { class: "label-tech", "Cache hit rate" }
                    strong { class: "value", "{hit_pct}" }
                }
                span { "{fmt_count(report.cache_read_tokens)} / {fmt_count(report.input_tokens)} input" }
            }
        }
        div { class: "activity-board", "data-default-layout": if default_layout { "true" } else { "false" },
            if featured {
                div { class: "activity-featured", for tile in featured_tiles { {tile} } }
            }
            for tile in tiles { {tile} }
        }
        div { class: "compact-kpi-grid activity-share-grid",
            for (label, rows) in breakdown_groups(&report) {
                {share_card(label, rows, on_breakdowns)}
            }
        }
        footer { class: "activity-footnote form-help",
            p { "API-equivalent USD is a list-price estimate, not billed spend. Reference prices as of {report.price_as_of}. {report.cache_write_assumption}" }
            p { "{fmt_count(report.matched_proxy_requests)} matched proxy · {fmt_count(report.bypassed_local_requests)} bypassed local · detail retention {report.detail_retention_days} days." }
            p {
                "{usd(report.cache_savings_usd)} estimated cache savings · {fmt_count(report.cache_read_tokens)} read · "
                "{fmt_count(report.cache_write_tokens)} write · {fmt_count(report.input_tokens.saturating_sub(report.cache_read_tokens))} uncached input."
            }
            if !unpriced.is_empty() {
                details { summary { "Unpriced models" } p { "{unpriced}" } }
            }
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
        "cache" => "Cache hit rate",
        "heatmap" => "Heatmap",
        "rhythm" => "Rhythm",
        _ => kind,
    }
}

fn breakdown_groups(report: &ActivityOverview) -> [(&'static str, &[BreakdownRow]); 4] {
    [
        ("Models", &report.by_model),
        ("Accounts", &report.by_account),
        ("Clients", &report.by_client),
        ("Model / Effort", &report.by_model_effort),
    ]
}

pub(super) fn top_breakdown_rows(rows: &[BreakdownRow], limit: usize) -> Vec<BreakdownRow> {
    let mut ranked: Vec<_> = rows.iter().collect();
    ranked.sort_by(|a, b| b.api_equivalent_cost_usd.total_cmp(&a.api_equivalent_cost_usd).then_with(|| b.requests.cmp(&a.requests)));
    ranked.into_iter().take(limit).cloned().collect()
}

fn share_card(label: &'static str, rows: &[BreakdownRow], on_breakdowns: EventHandler<()>) -> Element {
    let mask = try_consume_context::<PanelState>().is_some_and(|p| p.settings.read().mask_emails);
    let cost = rows.iter().map(|row| row.api_equivalent_cost_usd).sum();
    let requests = rows.iter().map(|row| row.requests as f64).sum();
    rsx! {
        article { key: "{label}", class: "glass-card stack activity-share-card",
            div { class: "row",
                h3 { "Top {label}" }
                button { class: "btn secondary", aria_label: "Full breakdown: {label}",
                    onclick: move |_| on_breakdowns.call(()), "Full breakdown →" }
            }
            if rows.is_empty() {
                p { class: "muted", "No activity in this group." }
            }
            for row in top_breakdown_rows(rows, 5) {
                {
                    let group = breakdown_group(label, &row, mask);
                    let hit = breakdown_cache_hit(&row);
                    let (metric, value, total, scale_label) = if row.api_equivalent_cost_usd > 0.0 {
                        (usd(row.api_equivalent_cost_usd), row.api_equivalent_cost_usd, cost, "of total API-equivalent cost")
                    } else {
                        (format!("{} req", fmt_count(row.requests)), row.requests as f64, requests, "of total requests")
                    };
                    rsx! {
                        div { key: "{row.key}", class: "activity-share-row",
                            div { class: "row", style: "flex-wrap: nowrap;",
                                span { class: "activity-share-label", title: "{group}", style: "flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;", "{group}" }
                                span { class: "chip muted", title: "Cache hit rate", aria_label: "Cache hit rate: {hit}", "{hit}" }
                            }
                            ShareBar { label: metric, value, total, scale_label }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Breakdowns(scope: ActivityScope) -> Element {
    let data = use_activity_overview(scope);
    read_view(data.read().as_ref(), breakdown_tables)
}

fn breakdown_tables(report: &ActivityOverview) -> Element {
    rsx! {
        section { class: "glass-card stack",
            h2 { "Activity breakdowns" }
            for (label, rows) in breakdown_groups(report) {
                section { key: "{label}", class: "stack",
                    h3 { "{label}" }
                    {breakdown(label, rows)}
                }
            }
        }
    }
}

fn breakdown_group(label: &str, row: &BreakdownRow, mask: bool) -> String {
    if label == "Accounts" && mask && row.label.contains('@') { mask_email(&row.label) } else { row.label.clone() }
}

fn breakdown_cache_hit(row: &BreakdownRow) -> String {
    let input = if row.input_tokens > 0 { row.input_tokens } else { row.total_tokens.saturating_sub(row.output_tokens) };
    fmt_hit_pct(effective_cache_hit_rate(input, row.cache_read_tokens, row.cache_hit_rate))
}

fn breakdown(label: &'static str, list: &[BreakdownRow]) -> Element {
    let mask = try_consume_context::<PanelState>().is_some_and(|p| p.settings.read().mask_emails);
    let requests = list.iter().map(|r| r.requests as f64).sum();
    let tokens = list.iter().map(|r| r.total_tokens as f64).sum();
    let cost = list.iter().map(|r| r.api_equivalent_cost_usd).sum();
    let rows = list.iter().map(|r| {
        let group = breakdown_group(label, r, mask);
        rsx! {
            tr { key: "{r.key}",
                th { scope: "row", "{group}" }
                td { ShareBar { label: fmt_count(r.requests), value: r.requests as f64, total: requests } }
                td { ShareBar { label: fmt_count(r.total_tokens), value: r.total_tokens as f64, total: tokens } }
                td { ShareBar { label: usd(r.api_equivalent_cost_usd), value: r.api_equivalent_cost_usd, total: cost } }
                td { "{fmt_count(r.failures)}" }
                td { "{fmt_count(r.output_tokens)}" }
                td { "{fmt_count(r.cache_read_tokens)}" }
                td { "{breakdown_cache_hit(r)}" }
                td { "{fmt_count(r.thinking_tokens)}" }
                td { "{fmt_count(r.unpriced_requests)}" }
            }
        }
    });
    rsx! { BreakdownTable { label, headers: &["Group", "Requests", "Tokens", "API-equivalent USD", "Failed", "Output",
    "Cache read", "Cache hit %", "Thinking", "Unpriced"], empty: list.is_empty(), children: rsx! { for row in rows { {row} } } } }
}

#[component]
fn BreakdownTable(label: &'static str, headers: &'static [&'static str], empty: bool, children: Element) -> Element {
    rsx! {
        if empty {
            p { class: "muted", "No rows for {label}." }
        } else {
            div { class: "provider-table activity-table", tabindex: "0", role: "region", aria_label: label,
                table {
                    caption { class: "sr-only", "{label}" }
                    thead { tr { for header in headers { th { scope: "col", "{header}" } } } }
                    tbody { {children} }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_buckets_follow_numeric_ranges() {
        let mut labels = ["2k–8k", "501–2k", "8k+", "0 (no thinking)", "1–500"];
        labels.sort_by_key(|label| thinking_bucket_order(label));
        assert_eq!(labels.map(thinking_bucket_label), ["0", "1–500", "501–2,000", "2,001–8,000", "8,001+"]);
        assert_eq!(thinking_bucket_label("custom"), "custom");
    }

    #[test]
    fn drift_status_distinguishes_coverage_from_a_verdict() {
        assert_eq!(drift_status(false, 0, 3), "Baseline warming up · 3 of 5 qualifying days");
        assert_eq!(drift_status(true, 0, 6), "Within normal band");
        assert_eq!(drift_status(true, 2, 6), "Drift detected (2 outlier days)");
    }

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
            rsx! { OverviewBoard { report, board, has_history: Some(true), on_reset: |_| {}, on_breakdowns: |_| {} } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("No requests matched the current filters"));
        assert!(rendered.contains("Reset filters"));
        assert!(!rendered.contains("compact-kpi-grid"));
        assert!(!rendered.contains("activity-board"));
    }

    fn overview_fixture(requests: u64, kinds: &[&str]) -> ActivityOverview {
        let board_tiles: Vec<_> = kinds
            .iter()
            .map(|kind| BoardTile { id: (*kind).into(), kind: (*kind).into(), metric: "requests".into(), span: 1, visible: true })
            .collect();
        serde_json::from_value(serde_json::json!({
            "api_equivalent_cost_usd": 0.0, "unpriced_requests": 0, "unpriced_models": [],
            "price_as_of": "", "cache_write_assumption": "", "total_tokens": 0,
            "input_tokens": 0, "output_tokens": 0, "cache_read_tokens": 0, "cache_write_tokens": 0,
            "thinking_tokens": 0, "requests": requests, "failures": 0, "cache_hit_rate": 0.0,
            "cache_savings_usd": 0.0, "matched_proxy_requests": 0, "bypassed_local_requests": 0,
            "trend": [], "heatmap": [], "rhythm": [], "by_model": [], "by_account": [],
            "by_client": [], "by_model_effort": [], "detail_retention_days": 7, "board_tiles": board_tiles
        }))
        .unwrap()
    }

    fn breakdown_fixture(label: &str, requests: u64, cost: f64) -> BreakdownRow {
        serde_json::from_value(serde_json::json!({
            "key": label, "label": label, "requests": requests, "failures": 0,
            "total_tokens": 120, "output_tokens": 20, "cache_read_tokens": 80,
            "thinking_tokens": 0, "api_equivalent_cost_usd": cost, "unpriced_requests": 0
        }))
        .unwrap()
    }

    #[test]
    fn top_breakdown_rows_ranks_and_caps_at_five() {
        let rows = [
            breakdown_fixture("Small", 1, 0.0),
            breakdown_fixture("Priced", 1, 3.0),
            breakdown_fixture("Zero", 4, 0.0),
            breakdown_fixture("Other priced", 2, 1.0),
            breakdown_fixture("Unpriced", 6, 0.0),
            breakdown_fixture("Rest", 2, 0.0),
        ];
        let top = top_breakdown_rows(&rows, 5);
        assert_eq!(top.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["Priced", "Other priced", "Unpriced", "Zero", "Rest"]);

        fn app() -> Element {
            let rows = [
                breakdown_fixture("Priced", 1, 3.0),
                breakdown_fixture("Other", 1, 1.0),
                breakdown_fixture("Unpriced", 6, 0.0),
                breakdown_fixture("Extra A", 1, 0.0),
                breakdown_fixture("Extra B", 1, 0.0),
                breakdown_fixture("Excluded", 1, 0.0),
            ];
            share_card("Models", &rows, EventHandler::new(|_| {}))
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("75.0% of total API-equivalent cost"));
        assert!(rendered.contains("54.5% of total requests"));
        assert!(!rendered.contains("Excluded"));
    }

    #[test]
    fn overview_omits_text_cache_and_stub_compare_tiles() {
        fn app() -> Element {
            let board = use_signal(|| None);
            let report =
                overview_fixture(1, &["trend", "cache", "heatmap", "rhythm", "models", "accounts", "clients", "model_effort", "compare"]);
            rsx! { OverviewBoard { report, board, has_history: Some(true), on_reset: |_| {}, on_breakdowns: |_| {} } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        for chart in ["Trend", "Cache hit rate", "Heatmap", "Rhythm"] {
            assert!(rendered.contains(&format!("\"{chart}\"")), "{chart} is missing");
        }
        assert_eq!(rendered.matches(NO_RANGE_DATA).count(), 4);
        assert!(!rendered.contains("cache-summary"));
        assert!(!rendered.contains("Open model / effort comparison"));
        assert!(!rendered.contains("No rows for"));
    }

    #[test]
    fn breakdowns_subtab_preserves_all_ten_columns() {
        fn app() -> Element {
            let mut report = overview_fixture(1, &[]);
            report.by_model = vec![breakdown_fixture("Example model", 1, 1.0)];
            report.by_account = vec![breakdown_fixture("Example account", 1, 1.0)];
            report.by_client = vec![breakdown_fixture("Example client", 1, 1.0)];
            report.by_model_effort = vec![breakdown_fixture("Example effort", 1, 1.0)];
            breakdown_tables(&report)
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        for header in
            ["Group", "Requests", "Tokens", "API-equivalent USD", "Failed", "Output", "Cache read", "Cache hit %", "Thinking", "Unpriced"]
        {
            assert_eq!(rendered.matches(&format!("\"{header}\"")).count(), 4, "{header}");
        }
        for row in ["Example model", "Example account", "Example client", "Example effort"] {
            assert!(rendered.contains(row));
        }
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
    let latency_client = client.clone();
    let latency_scope = scope.clone();
    let latency = use_resource(move || {
        let (client, scope) = (latency_client.clone(), latency_scope.clone());
        async move { timed_for(Duration::from_secs(25), client.activity_drift(&scope)).await }
    });
    let data = use_resource(move || {
        let client = client.clone();
        let scope = ActivityScope { baseline_key: optional(&baseline()), ..scope.clone() };
        async move { timed_for(Duration::from_secs(25), client.activity_compare(&scope)).await }
    });
    let result = data.read();
    let latency_result = latency.read();
    let latency_by_model: BTreeMap<_, _> = latency_result
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .into_iter()
        .flat_map(|report| &report.series)
        .filter_map(|s| {
            let median = median_daily_ttft(&s.days)?;
            Some(((s.model.as_str(), s.effort.as_deref()), median))
        })
        .collect();
    rsx! { section { class: "glass-card stack activity-comparisons",
        h2 { "Model / effort comparison" }
        p { class: "muted", "API list-price estimates, not billing. Task and retry metrics depend on retained detail." }
        {read_view(result.as_ref(), |report| {
            let mut options = vec![(String::new(), "Automatic".into())];
            options.extend(report.rows.iter().map(|r| (r.key.clone(), r.key.clone())));
            let ttft = |r: &CompareRow| latency_by_model.get(&(r.model.as_str(), r.effort.as_deref())).copied();
            let baseline_ttft = report.rows.iter().find(|r| Some(&r.key) == report.baseline_key.as_ref()).and_then(ttft);
            let max_ttft = report.rows.iter().filter_map(ttft).fold(0.0, f64::max);
            let total_cost = report.rows.iter().map(|r| r.api_equivalent_cost_usd).sum();
            rsx! {
                div { class: "comparison-baseline",
                    Choice { label: "Baseline", value: baseline(), options, change: move |v| baseline.set(v) }
                }
                p { "Effective baseline: {text(&report.baseline_key)} · pricing as of {report.price_as_of}" }
                p { class: "form-help",
                    "Bars show share of total listed cost and time to first token (TTFT) relative to the slowest row. TTFT is the median of the observed daily medians."
                    if latency_result.is_none() { " Loading latency…" }
                    else if latency_result.as_ref().is_some_and(Result::is_err) { " Latency could not be loaded." }
                }
                BreakdownTable { label: "Model and effort comparison", empty: report.rows.is_empty(), headers: &[
                    "Model", "Effort", "Reqs", "Est. cost (% share)", "$/req", "Cost vs base", "Median TTFT", "Fail %"
                ],
                    for r in &report.rows {
                        tr { key: "{r.key}",
                            th { scope: "row", "{r.model}" }
                            td { "{text(&r.effort)}" }
                            td { "{fmt_count(r.requests)}" }
                            td { ShareBar { label: usd(r.api_equivalent_cost_usd), value: r.api_equivalent_cost_usd, total: total_cost, show_share: true } }
                            td { "{money(r.mean_cost_per_request)}" }
                            td { {baseline_delta(r.baseline_cost_multiple)} }
                            td { class: "comparison-ttft-cell",
                                if let Some(ms) = ttft(r) {
                                    ShareBar { label: format!("{ms:.1} ms"), value: ms, total: max_ttft, scale_label: "of slowest listed value" }
                                } else { "—" }
                            }
                            td {
                                if r.requests > 0 { "{r.failures as f64 * 100.0 / r.requests as f64:.1}%" }
                                else { "—" }
                            }
                        }
                    }
                }
                details { summary { "Mean cost, latency deltas and other diagnostics" }
                    BreakdownTable { label: "Comparison diagnostics", empty: report.rows.is_empty(), headers: &[
                        "Model", "Effort", "Failed reqs", "TTFT vs base", "Mean cost/req", "Mean cost vs base",
                        "Cost/1M tokens", "Cache hit %", "Failed cost %", "Thinking/output %", "Fast/flex %", "Root tasks",
                        "Median task $", "Mean task $", "P90 task $", "Median tokens/task", "Median req/task", "Mixed tasks %", "Other models %", "Subagent $",
                        "Baseline cache Δ pp", "Detail coverage %", "$/1M input", "$/1M output", "$/1M cache read", "$/1M cache write (5m)"
                    ],
                    for r in &report.rows {
                        {
                            let latency = ttft(r);
                            let latency_multiple = latency.zip(baseline_ttft.filter(|n| *n > 0.0)).map(|(a, b)| a / b);
                            rsx! {
                                tr { key: "{r.key}",
                                    th { scope: "row", "{r.model}" }
                                    td { "{text(&r.effort)}" }
                                    td { "{fmt_count(r.failures)}" }
                                    td { {baseline_delta(latency_multiple)} }
                                    td { "{money(r.mean_cost_per_request)}" }
                                    td { {baseline_delta(r.baseline_req_cost_multiple)} }
                                    for cell in [
                                        money(r.cost_per_m_tokens), number(Some(r.cache_hit_pct)), number(Some(r.failed_cost_share_pct)),
                                        number(Some(r.thinking_output_share_pct)), number(Some(r.fast_flex_share_pct)), fmt_count(r.root_tasks),
                                        money(r.median_task_cost), money(r.mean_task_cost), money(r.p90_task_cost),
                                        r.median_tokens_per_task.map_or_else(|| "—".into(), fmt_count),
                                        r.median_requests_per_task.map_or_else(|| "—".into(), fmt_count),
                                        number(Some(r.mixed_task_share_pct)), number(Some(r.other_model_share_pct)), usd(r.subagent_cost_usd),
                                        number(r.baseline_cache_pp_delta), number(Some(r.detail_coverage_pct)),
                                        number(r.price.as_ref().map(|p| p.input_per_m)), number(r.price.as_ref().map(|p| p.output_per_m)),
                                        number(r.price.as_ref().map(|p| p.cache_read_per_m)), number(r.price.as_ref().map(|p| p.cache_write_5m_per_m)),
                                    ] { td { "{cell}" } }
                                }
                            }
                        }
                    }
                }
                }
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

fn median_daily_ttft(days: &[DriftDayPoint]) -> Option<f64> {
    let mut samples: Vec<_> = days.iter().filter_map(|d| d.median_ttft_ms).filter(|v| v.is_finite() && *v >= 0.0).collect();
    samples.sort_by(f64::total_cmp);
    let mid = samples.len() / 2;
    let upper = *samples.get(mid)?;
    Some(if samples.len().is_multiple_of(2) { f64::midpoint(samples[mid - 1], upper) } else { upper })
}

fn baseline_delta(multiple: Option<f64>) -> Element {
    let Some(pct) = multiple.map(|m| ((m - 1.0) * 100.0).round()).filter(|p| p.is_finite()) else { return rsx! {} };
    let (delta, tone) = if pct == 0.0 { ("0%".into(), "muted") } else { (format!("{pct:+.0}%"), if pct > 0.0 { "bad" } else { "ok" }) };
    rsx! { span { class: "chip {tone}", title: "Change versus selected baseline", aria_label: "{delta} versus selected baseline", "{delta}" } }
}

fn drift_status(has_judgment: bool, outliers: u64, qualifying: usize) -> String {
    match (has_judgment, outliers) {
        (_, 1) => "Drift detected (1 outlier day)".into(),
        (_, n) if n > 1 => format!("Drift detected ({n} outlier days)"),
        (true, _) => "Within normal band".into(),
        _ => format!("Baseline warming up · {qualifying} of 5 qualifying days"),
    }
}

#[component]
fn Drift(scope: ActivityScope, providers: Vec<ProviderSummary>) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let mut ttft = use_signal(|| false);
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
        p { class: "muted", "Thinking drift judgments require ≥30 replies/day and ≥5 qualifying prior days. Status reflects the detector’s tolerance around the reference band. TTFT is observed latency only; client changes show context, not cause." }
        div { class: "segmented", role: "group", aria_label: "Drift metric",
            button { class: "seg-btn", aria_pressed: !ttft(), onclick: move |_| ttft.set(false), "Thinking tokens" }
            button { class: "seg-btn", aria_pressed: ttft(), onclick: move |_| ttft.set(true), "Median TTFT" }
        }
        {read_view(result.as_ref(), |report| rsx! {
            if report.series.is_empty() { p { class: "muted", "No drift series in this scope." } }
            for s in &report.series {
                article { key: "{s.model}-{s.effort.as_deref().unwrap_or_default()}", class: "provider-inset stack",
                    h3 { "{s.model} · {text(&s.effort)}" }
                    if !ttft() {
                        p { "{drift_status(s.days.iter().any(|d| d.judgeable), s.outlier_days, s.days.iter().filter(|d| d.replies >= 30).count())}" }
                    }
                    DriftBandChart { days: s.days.clone(), markers: report.markers.clone(), ttft: ttft() }
                    details { summary { "Daily drift measurements" }
                        Table { label: "Daily drift", headers: &["Date", "Replies", "Thinking/reply", "Output/reply", "No thinking %", "Median TTFT (ms)", "Failure %", "Normal low", "Normal median", "Normal high", "Judgment"],
                            rows: s.days.iter().map(|d| vec![ d.date.clone(), d.replies.to_string(), number(Some(d.thinking_per_reply)), number(Some(d.output_per_reply)),
                                number(Some(d.no_thinking_share * 100.0)), number(d.median_ttft_ms), number(Some(d.failure_rate * 100.0)), number(d.baseline_low),
                                number(d.baseline_median_thinking), number(d.baseline_high),
                                if d.outlier { "Outlier" } else if d.judgeable { "Not an outlier" } else { "Insufficient coverage" }.into()
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
            {thinking_buckets("Thinking buckets", "Bucket", &r.buckets)}
            {thinking_buckets("Effort kinds", "Effort", &r.kinds)}
            Table { label: "Savings alternatives (estimates)", headers: &["Alternative", "Target model", "Effort", "Estimated cost", "Estimated savings", "Estimated savings %"],
                rows: r.alternatives.iter().map(|a| vec![format!("Estimate · {}", a.label), a.target_model.clone(), text(&a.target_effort),
                    usd(a.estimated_cost_usd), usd(a.estimated_savings_usd), number(Some(a.estimated_savings_pct))]).collect() }
            if !allowed.is_empty() { ThinkingProbe { providers: allowed.clone(), model_default: r.selected_model.clone(), effort_default: r.selected_effort.clone() } }
            h3 { "Thinking probe history" }
            Table { label: "Thinking test runs", headers: &["Ran", "Baseline", "Effort", "Comparison", "Compare effort", "Turns", "Judge", "Equivalence", "Measured cost Δ %", "Status"],
                rows: r.test_runs.iter().map(|t| vec![fmt_dt(t.ran_at), t.baseline_model.clone(), t.baseline_effort.clone(), t.compare_model.clone(), text(&t.compare_effort),
                    t.sampled_turns.to_string(), text(&t.judge_model), number(t.equivalence_rate), number(t.measured_cost_delta_pct), t.status.clone()]).collect() }
        })}
    } }
}

fn thinking_bucket_order(label: &str) -> usize {
    match thinking_bucket_label(label) {
        "0" => 0,
        "1–500" => 1,
        "501–2,000" => 501,
        "2,001–8,000" => 2001,
        "8,001+" => 8001,
        _ => usize::MAX,
    }
}

fn thinking_bucket_label(label: &str) -> &str {
    match label {
        "0 (no thinking)" => "0",
        "501–2k" => "501–2,000",
        "2k–8k" => "2,001–8,000",
        "8k+" => "8,001+",
        _ => label,
    }
}

fn thinking_buckets(label: &'static str, group: &'static str, rows: &[ThinkingBucket]) -> Element {
    let total = rows.iter().map(|b| b.estimated_thinking_cost_usd).sum();
    let mut rows: Vec<_> = rows.iter().collect();
    let headers: &'static [&'static str] = if group == "Bucket" {
        rows.sort_by_key(|b| thinking_bucket_order(&b.label));
        &["Bucket", "Turns", "Thinking", "Output", "Est. cost (% of thinking)"]
    } else {
        &["Effort", "Turns", "Thinking", "Output", "Est. cost (% of thinking)"]
    };
    rsx! {
        div { class: "thinking-costs",
        BreakdownTable { label, empty: rows.is_empty(), headers,
            for b in rows {
                tr { key: "{b.label}",
                    th { scope: "row", if group == "Bucket" { "{thinking_bucket_label(&b.label)}" } else { "{b.label}" } }
                    td { "{fmt_count(b.turns)}" }
                    td { "{fmt_count(b.thinking_tokens)}" }
                    td { "{fmt_count(b.output_tokens)}" }
                    td { ShareBar { label: usd(b.estimated_thinking_cost_usd), value: b.estimated_thinking_cost_usd, total,
                        scale_label: "of listed thinking cost", show_share: true } }
                }
            }
        }
        }
    }
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
        h3 { "Thinking probe checks" }
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
