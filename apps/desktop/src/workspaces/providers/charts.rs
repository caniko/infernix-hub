//! SVGs use measured/estimated contract data and have equivalent text tables.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeDelta, Utc};

use super::*;

pub(super) fn quota_values(state: QuotaObservationState, remaining: Option<f64>, used: Option<f64>) -> (Option<f64>, Option<f64>) {
    if !matches!(state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood) {
        return (None, None);
    }
    let remaining = remaining.filter(|v| v.is_finite());
    let used = used.filter(|v| v.is_finite());
    (remaining.or_else(|| used.map(|v| 100.0 - v)), used.or_else(|| remaining.map(|v| 100.0 - v)))
}

fn freshness(w: &QuotaWindow) -> (&'static str, &'static str) {
    match w.observation_state {
        QuotaObservationState::Measured if w.stale => ("Stale", "warn"),
        QuotaObservationState::Measured => ("Measured", "ok"),
        QuotaObservationState::StaleLastGood => ("Stale", "warn"),
        QuotaObservationState::Unsupported => ("Unsupported", "muted"),
        QuotaObservationState::ProbeFailed => ("Probe failed", "bad"),
        QuotaObservationState::Unavailable => ("Unavailable", "warn"),
        QuotaObservationState::NeverObserved => ("Not observed", "muted"),
    }
}

pub(super) fn reset_label(at: Option<DateTime<Utc>>) -> String {
    let Some(at) = at else {
        return "Reset not observed".into();
    };
    let minutes = (at - Utc::now()).num_minutes();
    let when = fmt_dt(at);
    if at <= Utc::now() { format!("Reset was due · {when}") } else { format!("Resets in {}h {}m · {when}", minutes / 60, minutes % 60) }
}

#[component]
pub(super) fn QuotaLimitBar(window: QuotaWindow) -> Element {
    let (remaining, used) = quota_values(window.observation_state, window.remaining_percent, window.used_percent);
    let (label, tone) = freshness(&window);
    rsx! {
        div { class: "quota-window",
            div { class: "row",
                strong { "{window.label}" }
                if let Some(model) = &window.scope_model { span { class: "muted", "{model}" } }
                Chip { label: label.to_string(), tone: Some(tone.to_string()) }
                span { class: "muted", "{window.status:?}" }
            }
            if let (Some(left), Some(used)) = (remaining, used) {
                Meter { left, used }
            } else if window.observation_state == QuotaObservationState::Measured {
                p { class: "muted", "Percentage not observed" }
            }
            if matches!(window.observation_state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood)
                && let Some(amount) = window.used_amount {
                p { {format!("{} / {} {}", number(Some(amount)), number(window.limit_amount), window.currency.clone().unwrap_or_else(|| format!("{:?}", window.unit)))} }
            }
            p { class: "muted", "{reset_label(window.resets_at)} · {window.cadence:?} · observed {date(window.observed_at)}" }
            if let Some(reason) = &window.error_reason { p { class: "muted", "{reason}" } }
        }
    }
}

#[component]
pub(super) fn Meter(left: f64, used: f64) -> Element {
    let width = left.clamp(0.0, 100.0);
    rsx! {
        svg { class: "quota-meter", view_box: "0 0 100 6", preserve_aspect_ratio: "none", "aria-hidden": "true",
            rect { width: "100", height: "6", class: "quota-track" }
            rect { width: "{width}", height: "6", class: "quota-left" }
        }
        span { class: "mono", "{left:.1}% left · {used:.1}% used" }
    }
}

type Point = (DateTime<Utc>, f64);

fn line(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let mut previous = None;
    points
        .iter()
        .map(|&(at, value)| {
            let move_to = previous.is_none_or(|prev| at - prev > TimeDelta::hours(4));
            previous = Some(at);
            format!("{}{:.2} {:.2}", if move_to { 'M' } else { 'L' }, x(at, start, end), y(value))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn x(at: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
    38.0 + 550.0 * (at - start).num_seconds() as f64 / (end - start).num_seconds().max(1) as f64
}

fn y(value: f64) -> f64 {
    132.0 - 120.0 * value.clamp(0.0, 100.0) / 100.0
}

#[component]
pub(super) fn QuotaHistoryChart(
    samples: Vec<QuotaSample>,
    forecasts: Vec<QuotaForecast>,
    resets: Vec<DateTime<Utc>>,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
    names: BTreeMap<String, String>,
) -> Element {
    let mut groups = BTreeMap::<&str, Vec<Point>>::new();
    for row in &samples {
        if row.observed_at >= from
            && row.observed_at <= until
            && !row.stale
            && row.observation_state == QuotaObservationState::Measured
            && let (Some(left), _) = quota_values(row.observation_state, row.remaining_percent, row.used_percent)
        {
            groups.entry(&row.account_id).or_default().push((row.observed_at, left));
        }
    }
    for points in groups.values_mut() {
        points.sort_by_key(|p| p.0);
    }
    let reset_times: BTreeSet<_> = resets.into_iter().filter(|t| *t >= from && *t <= until).collect();
    let reset_labels = reset_times.iter().map(|at| fmt_dt(*at)).collect::<Vec<_>>().join(", ");
    let name = |id: &str| names.get(id).map_or_else(|| id.to_string(), Clone::clone);
    rsx! {
        figure { class: "provider-chart",
            figcaption { "Remaining quota (%) · {fmt_dt(from)} → {fmt_dt(until)}" }
            if groups.is_empty() && forecasts.is_empty() { p { class: "muted", "No measured percentage samples for this window and time range." } }
            svg { view_box: "0 0 600 158", role: "img", "aria-label": "Measured remaining quota; dashed lines are estimated projections. Data table follows.",
                for pct in [0.0, 50.0, 100.0] {
                    line { x1: 38, x2: 588, y1: y(pct), y2: y(pct), class: "chart-gridline" }
                    text { x: 2, y: y(pct) + 4.0, "{pct}%" }
                }
                for &at in &reset_times {
                    line { x1: x(at, from, until), x2: x(at, from, until), y1: 12, y2: 132, class: "chart-reset",
                        title { "Reset: {fmt_dt(at)}" }
                    }
                }
                for (i, (id, points)) in groups.iter().enumerate() {
                    g { class: "chart-series chart-series-{i % 4}",
                        path { class: "chart-line", d: line(points, from, until), title { "{name(id)}" } }
                        for (at, left) in points {
                            circle { cx: x(*at, from, until), cy: y(*left), r: 2.0,
                                title { "{name(id)} · {fmt_dt(*at)} · {left:.1}% remaining" }
                            }
                        }
                    }
                }
                for forecast in &forecasts {
                    {
                        let points: Vec<_> = forecast.points.iter().filter(|p| p.at >= from && p.at <= until && p.remaining_percent.is_finite())
                            .map(|p| (p.at, p.remaining_percent)).collect();
                        // Projection is continuous between supplied forecast points.
                        let coords = points.iter().map(|(at, value)| format!("{:.2},{:.2}", x(*at, from, until), y(*value))).collect::<Vec<_>>().join(" ");
                        let label = forecast.account_id.as_deref().map(name).unwrap_or_else(|| "Pool".into());
                        rsx! {
                            polyline { class: "chart-line chart-projection", points: coords, title { "{label} · Estimated projection" } }
                            if let Some(at) = forecast.projected_exhaustion_at.filter(|at| *at >= from && *at <= until) {
                                line { x1: x(at, from, until), x2: x(at, from, until), y1: 12, y2: 132, class: "chart-exhaustion",
                                    title { "{label} · Projected exhaustion {fmt_dt(at)}" }
                                }
                            }
                        }
                    }
                }
            }
            if !forecasts.is_empty() {
                p { class: "label-tech", "Estimated projection · dashed · not a provider guarantee" }
                for forecast in &forecasts {
                    p { {format!("{} · {:.2} percentage points/hour · projected exhaustion {}", forecast.account_id.as_deref().map(name).unwrap_or_else(|| "Pool".into()), forecast.drain_pct_per_hour, date(forecast.projected_exhaustion_at))}
                        if forecast.resets_before_exhaustion { " · resets first" }
                    }
                }
                details {
                    summary { "Estimated projection data table" }
                    Table { label: "Estimated projection", headers: &["Scope", "Projected time", "Remaining %", "Used %"],
                        rows: forecasts.iter().flat_map(|forecast| {
                            let scope = forecast.account_id.as_deref().map(name).unwrap_or_else(|| "Pool".into());
                            forecast.points.iter().map(move |point| vec![
                                scope.clone(), fmt_dt(point.at), number(Some(point.remaining_percent)), number(Some(point.used_percent))
                            ])
                        }).collect() }
                }
            }
            if !reset_times.is_empty() {
                p { class: "muted", "Reset markers: {reset_labels}" }
            }
            details {
                summary { "Measured history data table" }
                Table { label: "Measured quota", headers: &["Account", "Observed", "Remaining %"],
                    rows: groups.iter().flat_map(|(id, points)| points.iter().map(|(at, val)| vec![name(id), fmt_dt(*at), number(Some(*val))])).collect() }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Metric {
    Requests,
    Tokens,
    Cost,
}

impl Metric {
    pub(super) fn parse(key: &str) -> Self {
        match key {
            "tokens" => Self::Tokens,
            "cost" => Self::Cost,
            _ => Self::Requests,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Requests => "Requests",
            Self::Tokens => "Tokens",
            Self::Cost => "API-equivalent cost (USD)",
        }
    }
    fn value(self, requests: u64, tokens: u64, cost: f64) -> f64 {
        match self {
            Self::Requests => requests as f64,
            Self::Tokens => tokens as f64,
            Self::Cost => cost,
        }
    }
}

fn bars(label: &'static str, metric: Metric, points: Vec<(String, f64)>) -> Element {
    let scale = points.iter().map(|p| p.1).fold(0.0, f64::max).max(f64::EPSILON);
    let step = 580.0 / points.len().max(1) as f64;
    let unit = metric.label();
    rsx! {
        figure { class: "provider-chart",
            figcaption { "{label} · {unit}" }
            if points.is_empty() { p { class: "muted", "No observations for this filter." } } else {
                svg { view_box: "0 0 600 134", role: "img", "aria-label": "{label}, {unit}. Data table follows.",
                    line { x1: 10, x2: 590, y1: 110, y2: 110, class: "chart-gridline" }
                    for (i, (bucket, value)) in points.iter().enumerate() {
                        {
                            let height = 100.0 * value.max(0.0) / scale;
                            rsx! { rect { x: 10.0 + i as f64 * step, y: 110.0 - height, width: step * 0.84, height, class: "chart-bar",
                                title { "{bucket} · {unit}: {value}" }
                            } }
                        }
                    }
                    text { x: 10, y: 128, "{points.first().map(|p| p.0.as_str()).unwrap_or_default()}" }
                    text { x: 590, y: 128, text_anchor: "end", "{points.last().map(|p| p.0.as_str()).unwrap_or_default()}" }
                }
                details { summary { "Data table" }
                Table { label, headers: &["Period", "Value"], rows: points.iter().map(|(key, value)| vec![key.clone(), value.to_string()]).collect() }
                }
            }
        }
    }
}

#[component]
pub(super) fn ActivityTrendChart(points: Vec<ActivityTrendPoint>, metric: Metric) -> Element {
    bars(
        "Daily activity",
        metric,
        points
            .into_iter()
            .map(|p| (p.label, metric.value(p.requests, p.input_tokens.saturating_add(p.output_tokens), p.api_equivalent_cost_usd)))
            .collect(),
    )
}

#[component]
pub(super) fn ActivityRhythmChart(points: Vec<RhythmBucket>, metric: Metric) -> Element {
    if points.is_empty() {
        return bars("Local-hour rhythm", metric, vec![]);
    }
    bars(
        "Local-hour rhythm",
        metric,
        (0..24)
            .map(|h| {
                (
                    format!("{h:02}:00"),
                    points.iter().find(|p| p.hour == h).map_or(0.0, |p| metric.value(p.requests, p.tokens, p.api_equivalent_cost_usd)),
                )
            })
            .collect(),
    )
}

#[component]
pub(super) fn ActivityHeatmap(cells: Vec<HeatmapCell>, metric: Metric) -> Element {
    let unit = metric.label();
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let values: BTreeMap<_, _> = cells
        .iter()
        .map(|c| ((usize::from(c.day_of_week), usize::from(c.hour)), metric.value(c.requests, c.tokens, c.api_equivalent_cost_usd)))
        .collect();
    let max = values.values().copied().fold(0.0, f64::max).max(f64::EPSILON);
    rsx! {
        figure { class: "provider-chart",
            figcaption { "Activity by weekday and local hour · {unit}" }
            svg { view_box: "0 0 610 178", role: "img", "aria-label": "Seven by twenty-four activity heatmap. Data table follows.",
                for hour in [0, 6, 12, 18, 23] { text { x: 35 + 24 * hour, y: 12, "{hour:02}" } }
                for (d, day) in days.into_iter().enumerate() {
                    text { x: 0, y: 33 + 22 * d, "{day}" }
                    for h in 0..24 {
                        {
                            let value = values.get(&(d, h)).copied().unwrap_or(0.0);
                            rsx! { rect { x: 34 + 24 * h, y: 18 + 22 * d, width: 21, height: 19, rx: 2, class: "chart-bar",
                                opacity: if value <= 0.0 { 0.13 } else { 0.25 + 0.75 * value / max },
                                title { "{day} {h:02}:00 · {unit}: {value}" }
                            } }
                        }
                    }
                }
            }
            if cells.is_empty() { p { class: "muted", "No activity observations in this scope." } }
            details { summary { "Recorded cells data table" }
                Table { label: "Heatmap cells", headers: &["Day", "Hour", "Value"], rows: values.iter().filter_map(|(&(d, h), v)| {
                    Some(vec![days.get(d)?.to_string(), format!("{h:02}:00"), v.to_string()])
                }).collect() }
            }
        }
    }
}

#[component]
pub(super) fn DriftBandChart(days: Vec<DriftDayPoint>, markers: Vec<DriftMarker>) -> Element {
    let max = days.iter().flat_map(|d| [d.thinking_per_reply, d.baseline_high.unwrap_or(0.0)]).fold(1.0, f64::max);
    let x = |i| 40.0 + 548.0 * i as f64 / days.len().saturating_sub(1).max(1) as f64;
    let y = |v| 130.0 - 112.0 * v / max;
    let band_path = |range: &[(usize, &DriftDayPoint)]| {
        let high = range.iter().map(|(i, d)| format!("{},{}", x(*i), y(d.baseline_high.unwrap_or_default())));
        let low = range.iter().rev().map(|(i, d)| format!("{},{}", x(*i), y(d.baseline_low.unwrap_or_default())));
        high.chain(low).collect::<Vec<_>>().join(" ")
    };
    let indexed: Vec<_> = days.iter().enumerate().collect();
    let groups = indexed.split(|(_, d)| d.baseline_low.is_none() || d.baseline_high.is_none());
    rsx! {
        figure { class: "provider-chart",
            figcaption { "Thinking tokens per reply · preceding 14-day normal band and median" }
            svg { view_box: "0 0 600 155", role: "img", "aria-label": "Daily thinking tokens, normal band, median, outliers and change markers. Daily data table follows.",
                line { x1: 40, x2: 588, y1: 130, y2: 130, class: "chart-gridline" }
                text { x: 2, y: 22, "{max:.0}" }
                text { x: 2, y: 132, "0" }
                for group in groups.filter(|group| !group.is_empty()) {
                    polygon { class: "chart-band", points: band_path(group) }
                }
                for (i, day) in days.iter().enumerate() {
                    if let Some(marker) = markers.iter().find(|m| m.date == day.date) {
                        line { x1: x(i), x2: x(i), y1: 13, y2: 130, class: "chart-reset", title { "{marker.label}" } }
                    }
                    if let Some(median) = day.baseline_median_thinking {
                        line { x1: x(i) - 3.0, x2: x(i) + 3.0, y1: y(median), y2: y(median), class: "chart-median" }
                    }
                    circle { cx: x(i), cy: y(day.thinking_per_reply), r: if day.outlier { "4" } else { "2.5" },
                        class: if day.outlier { "chart-outlier" } else { "chart-bar" },
                        title { "{day.date} · {day.thinking_per_reply:.1} thinking tokens per reply" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_quota_is_not_zero_and_measured_zero_is_valid() {
        assert_eq!(quota_values(QuotaObservationState::ProbeFailed, Some(0.0), None), (None, None));
        assert_eq!(quota_values(QuotaObservationState::Measured, Some(0.0), None), (Some(0.0), Some(100.0)));
    }

    #[test]
    fn quota_lines_break_only_after_four_hours() {
        let start = DateTime::UNIX_EPOCH;
        let end = start + TimeDelta::hours(12);
        let points = [(start, 90.0), (start + TimeDelta::hours(4), 80.0), (start + TimeDelta::hours(9), 70.0)];
        assert_eq!(line(&points, start, end).matches('M').count(), 2);
        assert_eq!(line(&points, start, end).matches('L').count(), 1);
    }
}
