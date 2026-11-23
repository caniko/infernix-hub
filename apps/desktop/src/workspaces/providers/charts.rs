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

pub(super) fn effective_cache_hit_rate(input_tokens: u64, cache_read_tokens: u64, _reported_rate: f64) -> Option<f64> {
    (input_tokens > 0).then(|| (cache_read_tokens as f64 / input_tokens as f64).min(1.0))
}

pub(super) fn request_cache_hit_rate(input_tokens: i64, cache_read_tokens: i64) -> Option<f64> {
    effective_cache_hit_rate(input_tokens.max(0) as u64, cache_read_tokens.max(0) as u64, 0.0)
}

pub(super) fn fmt_hit_pct(ratio: Option<f64>) -> String {
    ratio.filter(|r| r.is_finite()).map_or_else(|| "—".into(), |r| format!("{:.1}%", r.clamp(0.0, 1.0) * 100.0))
}

fn fmt_compact_axis(metric: Metric, value: f64) -> String {
    match metric {
        Metric::CacheHitRate => format!("{value:.0}%"),
        Metric::Cost => {
            if value >= 100.0 {
                format!("${value:.0}")
            } else if value >= 1.0 {
                format!("${value:.2}")
            } else {
                format!("${value:.3}")
            }
        }
        Metric::Requests | Metric::Tokens => {
            if value >= 1_000_000.0 {
                format!("{:.1}M", value / 1_000_000.0)
            } else if value >= 1_000.0 {
                format!("{:.1}k", value / 1_000.0)
            } else {
                format!("{value:.0}")
            }
        }
    }
}

fn clamp_tooltip_box(anchor_x: f64, anchor_y: f64, box_w: f64, box_h: f64, view_w: f64, min_y: f64) -> (f64, f64) {
    let bx = (anchor_x - box_w / 2.0).clamp(6.0, (view_w - box_w - 6.0).max(6.0));
    let above = anchor_y - box_h - 10.0;
    let by = if above >= min_y { above } else { (anchor_y + 12.0).max(min_y) };
    (bx, by)
}

#[derive(Clone, PartialEq)]
struct TooltipSpec {
    x: f64,
    y: f64,
    crosshair_x: Option<f64>,
    y_top: f64,
    y_bottom: f64,
    view_w: f64,
    title: String,
    detail: String,
    subdetail: Option<String>,
}

#[component]
fn SvgTooltip(spec: TooltipSpec) -> Element {
    let max_chars =
        spec.title.chars().count().max(spec.detail.chars().count()).max(spec.subdetail.as_ref().map_or(0, |s| s.chars().count()));
    let box_w = ((max_chars as f64) * 6.3 + 18.0).clamp(136.0, 290.0);
    let box_h = if spec.subdetail.is_some() { 50.0 } else { 36.0 };
    let (bx, by) = clamp_tooltip_box(spec.x, spec.y, box_w, box_h, spec.view_w, 4.0);
    rsx! {
        g { class: "chart-tooltip-layer", pointer_events: "none",
            if let Some(cx) = spec.crosshair_x {
                line { x1: "{cx:.2}", x2: "{cx:.2}", y1: "{spec.y_top:.2}", y2: "{spec.y_bottom:.2}", class: "chart-crosshair" }
            }
            circle { cx: "{spec.x:.2}", cy: "{spec.y:.2}", r: "5.5", class: "chart-point-halo" }
            rect { x: "{bx:.2}", y: "{by:.2}", width: "{box_w:.2}", height: "{box_h:.2}", rx: "6", class: "chart-tooltip-bg" }
            text { x: "{bx + 9.0:.2}", y: "{by + 14.0:.2}", class: "chart-tooltip-title", "{spec.title}" }
            text { x: "{bx + 9.0:.2}", y: "{by + 28.0:.2}", class: "chart-tooltip-detail", "{spec.detail}" }
            if let Some(sub) = &spec.subdetail {
                text { x: "{bx + 9.0:.2}", y: "{by + 42.0:.2}", class: "chart-tooltip-sub", "{sub}" }
            }
        }
    }
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
    let status_label = quota_status_label(window.status);
    let meta = match reset_cadence_label(window.cadence) {
        Some(cadence) => format!("{} · {cadence} · observed {}", reset_label(window.resets_at), date(window.observed_at)),
        None => format!("{} · observed {}", reset_label(window.resets_at), date(window.observed_at)),
    };
    rsx! {
        div { class: "quota-window",
            div { class: "row",
                strong { "{window.label}" }
                if let Some(model) = &window.scope_model { span { class: "muted", "{model}" } }
                Chip { label: label.to_string(), tone: Some(tone.to_string()) }
                if let Some(status) = status_label { span { class: "muted", "{status}" } }
            }
            if let (Some(left), Some(used)) = (remaining, used) {
                Meter { left, used }
            } else if window.observation_state == QuotaObservationState::Measured {
                p { class: "muted", "Percentage not observed" }
            }
            if matches!(window.observation_state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood)
                && let Some(amount) = window.used_amount {
                p { {format!("{} / {} {}", number(Some(amount)), number(window.limit_amount), window.currency.as_deref().unwrap_or_else(|| quota_unit_label(window.unit)))} }
            }
            p { class: "muted", "{meta}" }
            if let Some(reason) = &window.error_reason { p { class: "muted", "{reason}" } }
        }
    }
}

#[component]
pub(super) fn Meter(left: f64, used: f64) -> Element {
    let width = left.clamp(0.0, 100.0);
    let fill_class = if width < 10.0 {
        "quota-left bad"
    } else if width <= 30.0 {
        "quota-left warn"
    } else {
        "quota-left"
    };
    rsx! {
        svg { class: "quota-meter", view_box: "0 0 100 6", preserve_aspect_ratio: "none", "aria-hidden": "true",
            rect { width: "100", height: "6", rx: "3", class: "quota-track" }
            rect { width: "{width}", height: "6", rx: "3", class: "{fill_class}" }
        }
        span { class: "mono", "{left:.1}% left · {used:.1}% used" }
    }
}

type Point = (DateTime<Utc>, f64);

const QUOTA_W: f64 = 620.0;
const QUOTA_LEFT: f64 = 44.0;
const QUOTA_RIGHT: f64 = 604.0;
const QUOTA_TOP: f64 = 16.0;
const QUOTA_BOTTOM: f64 = 148.0;

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

fn area_segments(points: &[Point], start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<String> {
    let mut runs: Vec<Vec<Point>> = Vec::new();
    for &pt in points {
        let break_run = runs.last().and_then(|r| r.last()).is_none_or(|prev| pt.0 - prev.0 > TimeDelta::hours(4));
        if break_run {
            runs.push(vec![pt]);
        } else if let Some(last) = runs.last_mut() {
            last.push(pt);
        }
    }
    runs.into_iter()
        .filter(|run| run.len() >= 2)
        .map(|run| {
            let first_x = x(run[0].0, start, end);
            let last_x = x(run[run.len() - 1].0, start, end);
            let mut d = format!("M{first_x:.2} {QUOTA_BOTTOM:.2}");
            for (at, value) in &run {
                d.push_str(&format!(" L{:.2} {:.2}", x(*at, start, end), y(*value)));
            }
            d.push_str(&format!(" L{last_x:.2} {QUOTA_BOTTOM:.2} Z"));
            d
        })
        .collect()
}

fn x(at: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
    QUOTA_LEFT + (QUOTA_RIGHT - QUOTA_LEFT) * (at - start).num_seconds() as f64 / (end - start).num_seconds().max(1) as f64
}

fn y(value: f64) -> f64 {
    QUOTA_BOTTOM - (QUOTA_BOTTOM - QUOTA_TOP) * value.clamp(0.0, 100.0) / 100.0
}

#[derive(Clone, PartialEq)]
enum QuotaHover {
    Point {
        series_idx: usize,
        label: String,
        at: DateTime<Utc>,
        left: f64,
        cx: f64,
        cy: f64,
        projected: bool,
    },
    Segment {
        series_idx: usize,
        seg_idx: usize,
        label: String,
        from_at: DateTime<Utc>,
        from_left: f64,
        to_at: DateTime<Utc>,
        to_left: f64,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        projected: bool,
    },
    Marker {
        label: String,
        at: DateTime<Utc>,
        cx: f64,
        exhaustion: bool,
    },
}

impl QuotaHover {
    fn readout(&self) -> String {
        match self {
            Self::Point { label, at, left, projected, .. } => {
                let tag = if *projected { " (est.)" } else { "" };
                format!("{label}{tag} · {} · {left:.1}% left ({:.1}% used)", fmt_dt(*at), 100.0 - left)
            }
            Self::Segment { label, from_at, from_left, to_at, to_left, projected, .. } => {
                let delta = to_left - from_left;
                let tag = if *projected { " (est.)" } else { "" };
                format!("{label}{tag} · {from_left:.1}% → {to_left:.1}% ({delta:+.1} pp) · {} → {}", fmt_dt(*from_at), fmt_dt(*to_at))
            }
            Self::Marker { label, at, exhaustion, .. } => {
                if *exhaustion {
                    format!("{label} · Projected exhaustion {}", fmt_dt(*at))
                } else {
                    format!("Quota reset · {}", fmt_dt(*at))
                }
            }
        }
    }

    fn tooltip(&self) -> TooltipSpec {
        match self {
            Self::Point { label, at, left, cx, cy, projected, .. } => TooltipSpec {
                x: *cx,
                y: *cy,
                crosshair_x: Some(*cx),
                y_top: QUOTA_TOP,
                y_bottom: QUOTA_BOTTOM,
                view_w: QUOTA_W,
                title: if *projected { format!("{label} (projected)") } else { label.clone() },
                detail: format!("{left:.1}% left · {:.1}% used", 100.0 - left),
                subdetail: Some(fmt_dt(*at)),
            },
            Self::Segment { label, from_at, from_left, to_at, to_left, x1, y1, x2, y2, projected, .. } => {
                let mid_x = (x1 + x2) / 2.0;
                let mid_y = (y1 + y2) / 2.0;
                let delta = to_left - from_left;
                TooltipSpec {
                    x: mid_x,
                    y: mid_y,
                    crosshair_x: Some(mid_x),
                    y_top: QUOTA_TOP,
                    y_bottom: QUOTA_BOTTOM,
                    view_w: QUOTA_W,
                    title: if *projected { format!("{label} (projected segment)") } else { format!("{label} · segment") },
                    detail: format!("{from_left:.1}% → {to_left:.1}% ({delta:+.1} pp)"),
                    subdetail: Some(format!("{} → {}", fmt_dt(*from_at), fmt_dt(*to_at))),
                }
            }
            Self::Marker { label, at, cx, exhaustion } => TooltipSpec {
                x: *cx,
                y: QUOTA_TOP + 24.0,
                crosshair_x: Some(*cx),
                y_top: QUOTA_TOP,
                y_bottom: QUOTA_BOTTOM,
                view_w: QUOTA_W,
                title: if *exhaustion { format!("{label} · exhaustion") } else { "Quota reset".into() },
                detail: fmt_dt(*at),
                subdetail: None,
            },
        }
    }
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
    let mut hovered = use_signal(|| None::<QuotaHover>);
    let grad_id = use_hook(|| format!("chart-quota-fill-{}", Uuid::new_v4().simple()));
    let grad_url = format!("url(#{grad_id})");
    let mut groups = BTreeMap::<String, Vec<Point>>::new();
    for row in &samples {
        if row.observed_at >= from
            && row.observed_at <= until
            && !row.stale
            && row.observation_state == QuotaObservationState::Measured
            && let (Some(left), _) = quota_values(row.observation_state, row.remaining_percent, row.used_percent)
        {
            groups.entry(row.account_id.clone()).or_default().push((row.observed_at, left));
        }
    }
    for points in groups.values_mut() {
        points.sort_by_key(|p| p.0);
    }
    let series_list: Vec<(String, String, Vec<Point>)> = groups
        .into_iter()
        .map(|(id, pts)| {
            let label = names.get(&id).cloned().unwrap_or_else(|| id.clone());
            (id, label, pts)
        })
        .collect();
    let reset_times: BTreeSet<_> = resets.into_iter().filter(|t| *t >= from && *t <= until).collect();
    let reset_labels = reset_times.iter().map(|at| fmt_dt(*at)).collect::<Vec<_>>().join(", ");
    let mid_time = from + TimeDelta::seconds((until - from).num_seconds() / 2);
    let name = |id: &str| names.get(id).map_or_else(|| id.to_string(), Clone::clone);
    let active = hovered();
    rsx! {
        figure { class: "provider-chart chart-stage",
            div { class: "chart-header",
                figcaption { "Remaining quota (%) · {fmt_dt(from)} → {fmt_dt(until)}" }
                span { class: if active.is_some() { "chart-readout active mono" } else { "chart-readout mono" },
                    {active.as_ref().map_or_else(|| "Hover a line, segment, or point to inspect".to_owned(), QuotaHover::readout)}
                }
            }
            if !series_list.is_empty() || !forecasts.is_empty() {
                div { class: "chart-legend",
                    for (i, (_, label, _)) in series_list.iter().enumerate() {
                        span { key: "leg-{i}", class: "chart-legend-item chart-series-{i % 4}",
                            span { class: "chart-swatch" }
                            "{label}"
                        }
                    }
                    if !forecasts.is_empty() {
                        span { class: "chart-legend-item",
                            span { class: "chart-swatch dashed" }
                            "Estimated projection"
                        }
                    }
                    if !reset_times.is_empty() {
                        span { class: "chart-legend-item",
                            span { class: "chart-swatch reset" }
                            "Reset"
                        }
                    }
                }
            }
            if series_list.is_empty() && forecasts.is_empty() {
                p { class: "muted", "No measured percentage samples for this window and time range." }
            }
            svg {
                view_box: "0 0 620 174",
                role: "img",
                "aria-label": "Measured remaining quota; dashed lines are estimated projections. Data table follows.",
                onmouseleave: move |_| hovered.set(None),
                defs {
                    linearGradient { id: "{grad_id}", x1: "0", y1: "0", x2: "0", y2: "1",
                        stop { offset: "0%", stop_color: "var(--honey)", stop_opacity: "0.22" }
                        stop { offset: "100%", stop_color: "var(--honey)", stop_opacity: "0.0" }
                    }
                }
                for pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
                    line {
                        x1: "{QUOTA_LEFT}",
                        x2: "{QUOTA_RIGHT}",
                        y1: "{y(pct):.2}",
                        y2: "{y(pct):.2}",
                        class: if pct == 0.0 || pct == 100.0 { "chart-gridline" } else { "chart-gridline subtle" },
                    }
                    if pct == 0.0 || pct == 50.0 || pct == 100.0 {
                        text { x: "{QUOTA_LEFT - 6.0}", y: "{y(pct) + 3.5:.2}", text_anchor: "end", "{pct:.0}%" }
                    }
                }
                text { x: "{QUOTA_LEFT}", y: "166", "{fmt_dt(from)}" }
                text { x: "{(QUOTA_LEFT + QUOTA_RIGHT) / 2.0:.1}", y: "166", text_anchor: "middle", "{fmt_dt(mid_time)}" }
                text { x: "{QUOTA_RIGHT}", y: "166", text_anchor: "end", "{fmt_dt(until)}" }
                for &at in &reset_times {
                    {
                        let rx = x(at, from, until);
                        rsx! {
                            line { x1: "{rx:.2}", x2: "{rx:.2}", y1: "{QUOTA_TOP}", y2: "{QUOTA_BOTTOM}", class: "chart-reset",
                                title { "Reset: {fmt_dt(at)}" }
                            }
                            line {
                                x1: "{rx:.2}", x2: "{rx:.2}", y1: "{QUOTA_TOP}", y2: "{QUOTA_BOTTOM}",
                                class: "chart-hit",
                                stroke: "transparent",
                                stroke_width: "10",
                                onmouseenter: move |_| hovered.set(Some(QuotaHover::Marker { label: "Reset".into(), at, cx: rx, exhaustion: false })),
                            }
                        }
                    }
                }
                for (i, (_, label, points)) in series_list.iter().enumerate() {
                    g { key: "series-{i}", class: "chart-series chart-series-{i % 4}",
                        if series_list.len() == 1 {
                            for area_d in area_segments(points, from, until) {
                                path { d: "{area_d}", fill: "{grad_url}", pointer_events: "none" }
                            }
                        }
                        path { class: "chart-line", d: line(points, from, until), title { "{label}" } }
                        for (seg_idx, pair) in points.windows(2).enumerate() {
                            if pair[1].0 - pair[0].0 <= TimeDelta::hours(4) {
                                {
                                    let x1 = x(pair[0].0, from, until);
                                    let y1 = y(pair[0].1);
                                    let x2 = x(pair[1].0, from, until);
                                    let y2 = y(pair[1].1);
                                    let is_seg = matches!(active, Some(QuotaHover::Segment { series_idx, seg_idx: s, projected: false, .. }) if series_idx == i && s == seg_idx);
                                    let seg_label = label.clone();
                                    let p0 = pair[0];
                                    let p1 = pair[1];
                                    rsx! {
                                        if is_seg {
                                            line { x1: "{x1:.2}", y1: "{y1:.2}", x2: "{x2:.2}", y2: "{y2:.2}", class: "chart-line chart-segment-active", pointer_events: "none" }
                                        }
                                        line {
                                            x1: "{x1:.2}", y1: "{y1:.2}", x2: "{x2:.2}", y2: "{y2:.2}",
                                            class: "chart-hit",
                                            stroke: "transparent",
                                            stroke_width: "12",
                                            onmouseenter: move |_| hovered.set(Some(QuotaHover::Segment {
                                                series_idx: i,
                                                seg_idx,
                                                label: seg_label.clone(),
                                                from_at: p0.0,
                                                from_left: p0.1,
                                                to_at: p1.0,
                                                to_left: p1.1,
                                                x1,
                                                y1,
                                                x2,
                                                y2,
                                                projected: false,
                                            })),
                                        }
                                    }
                                }
                            }
                        }
                        for &(at, left) in points {
                            {
                                let cx = x(at, from, until);
                                let cy = y(left);
                                let is_pt = matches!(active, Some(QuotaHover::Point { series_idx, at: a, projected: false, .. }) if series_idx == i && a == at);
                                let pt_label = label.clone();
                                rsx! {
                                    circle {
                                        cx: "{cx:.2}",
                                        cy: "{cy:.2}",
                                        r: if is_pt { "4.2" } else { "2.4" },
                                        class: if is_pt { "chart-point active" } else { "chart-point" },
                                        title { "{label} · {fmt_dt(at)} · {left:.1}% remaining" }
                                    }
                                    circle {
                                        cx: "{cx:.2}",
                                        cy: "{cy:.2}",
                                        r: "7.5",
                                        fill: "transparent",
                                        class: "chart-hit",
                                        onmouseenter: move |_| hovered.set(Some(QuotaHover::Point {
                                            series_idx: i,
                                            label: pt_label.clone(),
                                            at,
                                            left,
                                            cx,
                                            cy,
                                            projected: false,
                                        })),
                                    }
                                }
                            }
                        }
                    }
                }
                for (fi, forecast) in forecasts.iter().enumerate() {
                    {
                        let points: Vec<_> = forecast.points.iter().filter(|p| p.at >= from && p.at <= until && p.remaining_percent.is_finite())
                            .map(|p| (p.at, p.remaining_percent)).collect();
                        let coords = points.iter().map(|(at, value)| format!("{:.2},{:.2}", x(*at, from, until), y(*value))).collect::<Vec<_>>().join(" ");
                        let label = forecast.account_id.as_deref().map(name).unwrap_or_else(|| "Pool".into());
                        let exh_label = label.clone();
                        rsx! {
                            polyline { class: "chart-line chart-projection", points: coords, title { "{label} · Estimated projection" } }
                            for (seg_idx, pair) in points.windows(2).enumerate() {
                                {
                                    let x1 = x(pair[0].0, from, until);
                                    let y1 = y(pair[0].1);
                                    let x2 = x(pair[1].0, from, until);
                                    let y2 = y(pair[1].1);
                                    let seg_label = label.clone();
                                    let p0 = pair[0];
                                    let p1 = pair[1];
                                    rsx! {
                                        line {
                                            x1: "{x1:.2}", y1: "{y1:.2}", x2: "{x2:.2}", y2: "{y2:.2}",
                                            class: "chart-hit",
                                            stroke: "transparent",
                                            stroke_width: "12",
                                            onmouseenter: move |_| hovered.set(Some(QuotaHover::Segment {
                                                series_idx: fi,
                                                seg_idx,
                                                label: seg_label.clone(),
                                                from_at: p0.0,
                                                from_left: p0.1,
                                                to_at: p1.0,
                                                to_left: p1.1,
                                                x1,
                                                y1,
                                                x2,
                                                y2,
                                                projected: true,
                                            })),
                                        }
                                    }
                                }
                            }
                            for &(at, left) in &points {
                                {
                                    let cx = x(at, from, until);
                                    let cy = y(left);
                                    let pt_label = label.clone();
                                    rsx! {
                                        circle {
                                            cx: "{cx:.2}", cy: "{cy:.2}", r: "6.5", fill: "transparent", class: "chart-hit",
                                            onmouseenter: move |_| hovered.set(Some(QuotaHover::Point {
                                                series_idx: fi,
                                                label: pt_label.clone(),
                                                at,
                                                left,
                                                cx,
                                                cy,
                                                projected: true,
                                            })),
                                        }
                                    }
                                }
                            }
                            if let Some(at) = forecast.projected_exhaustion_at.filter(|at| *at >= from && *at <= until) {
                                {
                                    let ex = x(at, from, until);
                                    rsx! {
                                        line { x1: "{ex:.2}", x2: "{ex:.2}", y1: "{QUOTA_TOP}", y2: "{QUOTA_BOTTOM}", class: "chart-exhaustion",
                                            title { "{exh_label} · Projected exhaustion {fmt_dt(at)}" }
                                        }
                                        line {
                                            x1: "{ex:.2}", x2: "{ex:.2}", y1: "{QUOTA_TOP}", y2: "{QUOTA_BOTTOM}",
                                            class: "chart-hit",
                                            stroke: "transparent",
                                            stroke_width: "10",
                                            onmouseenter: move |_| hovered.set(Some(QuotaHover::Marker {
                                                label: exh_label.clone(),
                                                at,
                                                cx: ex,
                                                exhaustion: true,
                                            })),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(spec) = active.as_ref().map(QuotaHover::tooltip) {
                    SvgTooltip { spec }
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
                    rows: series_list.iter().flat_map(|(_, label, points)| points.iter().map(move |(at, val)| vec![label.clone(), fmt_dt(*at), number(Some(*val))])).collect() }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Metric {
    Requests,
    Tokens,
    Cost,
    CacheHitRate,
}

impl Metric {
    pub(super) fn parse(key: &str) -> Self {
        match key {
            "tokens" => Self::Tokens,
            "cost" => Self::Cost,
            "cache_hit" => Self::CacheHitRate,
            _ => Self::Requests,
        }
    }
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Requests => "Requests",
            Self::Tokens => "Tokens",
            Self::Cost => "API-equivalent cost (USD)",
            Self::CacheHitRate => "Cache hit rate (%)",
        }
    }
    fn value(self, requests: u64, tokens: u64, cost: f64, cache_hit_rate: Option<f64>) -> Option<f64> {
        Some(match self {
            Self::Requests => requests as f64,
            Self::Tokens => tokens as f64,
            Self::Cost => cost,
            Self::CacheHitRate => cache_hit_rate? * 100.0,
        })
    }
    fn format_value(self, value: Option<f64>) -> String {
        let Some(value) = value else {
            return "—".into();
        };
        match self {
            Self::Requests | Self::Tokens => fmt_count(value.max(0.0).round() as u64),
            Self::Cost => format!("${value:.4}"),
            Self::CacheHitRate => format!("{value:.1}%"),
        }
    }
}

#[derive(Clone, PartialEq)]
struct BarItem {
    label: String,
    value: Option<f64>,
    requests: u64,
    failures: u64,
    tokens: u64,
    cache_read: u64,
    cache_hit_rate: Option<f64>,
    cost_usd: f64,
}

#[component]
fn InteractiveBars(label: &'static str, metric: Metric, items: Vec<BarItem>) -> Element {
    let mut hovered = use_signal(|| None::<usize>);
    let grad_prefix = use_hook(|| Uuid::new_v4().simple().to_string());
    let accent_grad_id = format!("chart-bar-accent-{grad_prefix}");
    let cache_grad_id = format!("chart-bar-cache-{grad_prefix}");
    let view_w = 620.0_f64;
    let plot_left = 48.0_f64;
    let plot_right = 604.0_f64;
    let plot_top = 18.0_f64;
    let plot_bottom = 132.0_f64;
    let plot_w = plot_right - plot_left;
    let plot_h = plot_bottom - plot_top;
    let scale =
        if metric == Metric::CacheHitRate { 100.0 } else { items.iter().filter_map(|p| p.value).fold(0.0, f64::max).max(f64::EPSILON) };
    let step = plot_w / items.len().max(1) as f64;
    let unit = metric.label();
    let active_idx = hovered().filter(|&i| i < items.len());
    let grad_url = if metric == Metric::CacheHitRate { format!("url(#{cache_grad_id})") } else { format!("url(#{accent_grad_id})") };
    let readout = active_idx.map(|i| {
        let p = &items[i];
        format!(
            "{} · {} · {} req · {} tok · {} cache",
            p.label,
            metric.format_value(p.value),
            fmt_count(p.requests),
            fmt_count(p.tokens),
            fmt_hit_pct(p.cache_hit_rate)
        )
    });
    rsx! {
        figure { class: "provider-chart chart-stage",
            div { class: "chart-header",
                figcaption { "{label} · {unit}" }
                span { class: if readout.is_some() { "chart-readout active mono" } else { "chart-readout mono" },
                    {readout.as_deref().unwrap_or("Hover a bar to inspect")}
                }
            }
            if items.is_empty() {
                p { class: "muted", "No observations for this filter." }
            } else {
                svg {
                    view_box: "0 0 620 156",
                    role: "img",
                    "aria-label": "{label}, {unit}. Data table follows.",
                    onmouseleave: move |_| hovered.set(None),
                    defs {
                        linearGradient { id: "{accent_grad_id}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "var(--honey)", stop_opacity: "0.95" }
                            stop { offset: "100%", stop_color: "var(--honey)", stop_opacity: "0.45" }
                        }
                        linearGradient { id: "{cache_grad_id}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "var(--ok)", stop_opacity: "0.95" }
                            stop { offset: "100%", stop_color: "var(--ok)", stop_opacity: "0.45" }
                        }
                    }
                    for frac in [0.0, 0.5, 1.0] {
                        {
                            let gy = plot_bottom - plot_h * frac;
                            let tick = fmt_compact_axis(metric, scale * frac);
                            rsx! {
                                line {
                                    x1: "{plot_left}",
                                    x2: "{plot_right}",
                                    y1: "{gy:.2}",
                                    y2: "{gy:.2}",
                                    class: if frac == 0.0 { "chart-gridline" } else { "chart-gridline subtle" },
                                }
                                text { x: "{plot_left - 6.0}", y: "{gy + 3.5:.2}", text_anchor: "end", "{tick}" }
                            }
                        }
                    }
                    for (i, item) in items.iter().enumerate() {
                        {
                            let col_x = plot_left + i as f64 * step;
                            let bar_w = (step * 0.78).max(2.0);
                            let bar_x = col_x + (step - bar_w) / 2.0;
                            let height = (plot_h * item.value.unwrap_or_default() / scale).clamp(0.0, plot_h);
                            let bar_y = plot_bottom - height;
                            let is_active = active_idx == Some(i);
                            rsx! {
                                if is_active {
                                    rect { x: "{col_x:.2}", y: "{plot_top}", width: "{step:.2}", height: "{plot_h}", class: "chart-col-highlight", pointer_events: "none" }
                                }
                                if item.value.is_some() {
                                    rect {
                                        x: "{bar_x:.2}",
                                        y: "{bar_y:.2}",
                                        width: "{bar_w:.2}",
                                        height: "{height:.2}",
                                        rx: "2.5",
                                        fill: "{grad_url}",
                                        class: if is_active { "chart-bar chart-bar-active" } else { "chart-bar" },
                                        pointer_events: "none",
                                    }
                                }
                                rect {
                                    x: "{col_x:.2}",
                                    y: "{plot_top}",
                                    width: "{step:.2}",
                                    height: "{plot_h}",
                                    fill: "transparent",
                                    class: "chart-hit",
                                    onmouseenter: move |_| hovered.set(Some(i)),
                                    title { "{item.label} · {unit}: {metric.format_value(item.value)}" }
                                }
                            }
                        }
                    }
                    text { x: "{plot_left}", y: "149", "{items.first().map(|p| p.label.as_str()).unwrap_or_default()}" }
                    if items.len() > 2 {
                        text { x: "{(plot_left + plot_right) / 2.0:.1}", y: "149", text_anchor: "middle", "{items[items.len() / 2].label}" }
                    }
                    text { x: "{plot_right}", y: "149", text_anchor: "end", "{items.last().map(|p| p.label.as_str()).unwrap_or_default()}" }
                    if let Some(i) = active_idx {
                        {
                            let item = &items[i];
                            let cx = plot_left + (i as f64 + 0.5) * step;
                            let height = (plot_h * item.value.unwrap_or_default() / scale).clamp(0.0, plot_h);
                            let cy = plot_bottom - height;
                            let fail_part = if item.failures > 0 { format!(" · {} fail", fmt_count(item.failures)) } else { String::new() };
                            let spec = TooltipSpec {
                                x: cx,
                                y: cy,
                                crosshair_x: Some(cx),
                                y_top: plot_top,
                                y_bottom: plot_bottom,
                                view_w,
                                title: format!("{} · {}", item.label, metric.format_value(item.value)),
                                detail: format!("{} req{fail_part} · {} tok", fmt_count(item.requests), fmt_count(item.tokens)),
                                subdetail: Some(format!("Cache {} ({} read) · ${:.4}", fmt_hit_pct(item.cache_hit_rate), fmt_count(item.cache_read), item.cost_usd)),
                            };
                            rsx! { SvgTooltip { spec } }
                        }
                    }
                }
                details { summary { "Data table" }
                    Table { label, headers: &["Period", "Value", "Requests", "Tokens", "Cache hit %", "API-equivalent USD"],
                        rows: items.iter().map(|p| vec![
                            p.label.clone(),
                            metric.format_value(p.value),
                            fmt_count(p.requests),
                            fmt_count(p.tokens),
                            fmt_hit_pct(p.cache_hit_rate),
                            format!("${:.4}", p.cost_usd),
                        ]).collect()
                    }
                }
            }
        }
    }
}

#[component]
pub(super) fn ActivityTrendChart(points: Vec<ActivityTrendPoint>, metric: Metric) -> Element {
    let mut points = points;
    points.sort_by_key(|p| p.bucket_start);
    // Hub daily buckets use UTC dates and %m-%d labels. Leave longer spans sparse.
    let fill_gaps = points
        .first()
        .zip(points.last())
        .is_some_and(|(first, last)| last.bucket_start.date_naive() - first.bucket_start.date_naive() <= TimeDelta::days(400));
    let mut items = Vec::with_capacity(points.len());
    let mut previous_day = None;
    for p in points {
        let day = p.bucket_start.date_naive();
        if fill_gaps && let Some(previous) = previous_day {
            for offset in 1..day.signed_duration_since(previous).num_days() {
                items.push(BarItem {
                    label: (previous + TimeDelta::days(offset)).format("%m-%d").to_string(),
                    value: metric.value(0, 0, 0.0, None),
                    requests: 0,
                    failures: 0,
                    tokens: 0,
                    cache_read: 0,
                    cache_hit_rate: None,
                    cost_usd: 0.0,
                });
            }
        }
        previous_day = Some(day);
        let tokens = p.input_tokens.saturating_add(p.output_tokens);
        let cache_hit_rate = effective_cache_hit_rate(p.input_tokens, p.cache_read_tokens, p.cache_hit_rate);
        items.push(BarItem {
            label: p.label,
            value: metric.value(p.requests, tokens, p.api_equivalent_cost_usd, cache_hit_rate),
            requests: p.requests,
            failures: p.failures,
            tokens,
            cache_read: p.cache_read_tokens,
            cache_hit_rate,
            cost_usd: p.api_equivalent_cost_usd,
        });
    }
    rsx! { InteractiveBars { label: "Daily activity", metric, items } }
}

#[component]
pub(super) fn ActivityRhythmChart(points: Vec<RhythmBucket>, metric: Metric) -> Element {
    if points.is_empty() {
        return rsx! { InteractiveBars { label: "Local-hour rhythm", metric, items: vec![] } };
    }
    let items = (0..24)
        .map(|h| match points.iter().find(|p| p.hour == h) {
            Some(p) => {
                let cache_hit_rate = effective_cache_hit_rate(p.input_tokens, p.cache_read_tokens, p.cache_hit_rate);
                BarItem {
                    label: format!("{h:02}:00"),
                    value: metric.value(p.requests, p.tokens, p.api_equivalent_cost_usd, cache_hit_rate),
                    requests: p.requests,
                    failures: 0,
                    tokens: p.tokens,
                    cache_read: p.cache_read_tokens,
                    cache_hit_rate,
                    cost_usd: p.api_equivalent_cost_usd,
                }
            }
            None => BarItem {
                label: format!("{h:02}:00"),
                value: metric.value(0, 0, 0.0, None),
                requests: 0,
                failures: 0,
                tokens: 0,
                cache_read: 0,
                cache_hit_rate: None,
                cost_usd: 0.0,
            },
        })
        .collect();
    rsx! { InteractiveBars { label: "Local-hour rhythm", metric, items } }
}

#[derive(Clone, PartialEq)]
enum CacheHover {
    Point(usize),
    Segment(usize),
}

#[component]
pub(super) fn CacheHitRateChart(points: Vec<ActivityTrendPoint>) -> Element {
    let mut hovered = use_signal(|| None::<CacheHover>);
    let grad_id = use_hook(|| format!("chart-cache-area-{}", Uuid::new_v4().simple()));
    let grad_url = format!("url(#{grad_id})");
    let view_w = 620.0_f64;
    let plot_left = 44.0_f64;
    let plot_right = 604.0_f64;
    let plot_top = 16.0_f64;
    let plot_bottom = 134.0_f64;
    let plot_h = plot_bottom - plot_top;
    let mut points = points;
    points.sort_by_key(|p| p.bucket_start);
    let n = points.len();
    let px = |i: usize| match (points.first(), points.last()) {
        (Some(first), Some(last)) if first.bucket_start != last.bucket_start => {
            let elapsed = (points[i].bucket_start - first.bucket_start).as_seconds_f64();
            let span = (last.bucket_start - first.bucket_start).as_seconds_f64();
            plot_left + (plot_right - plot_left) * elapsed / span
        }
        _ => (plot_left + plot_right) / 2.0,
    };
    let py = |rate: f64| plot_bottom - plot_h * rate.clamp(0.0, 1.0);
    let rates: Vec<_> = points.iter().map(|p| effective_cache_hit_rate(p.input_tokens, p.cache_read_tokens, p.cache_hit_rate)).collect();
    let plotted: Vec<_> = rates.iter().enumerate().filter_map(|(i, rate)| Some((i, (*rate)?))).collect();
    let (weighted_rate, total_in) = plotted.iter().fold((0.0, 0.0), |(sum, total), &(i, rate)| {
        let input = points[i].input_tokens as f64;
        (sum + rate * input, total + input)
    });
    let avg_rate = (total_in > 0.0).then(|| weighted_rate / total_in);

    // Use the same runs for visible paths and interactive segments.
    let runs = plotted.chunk_by(|&(a, _), &(b, _)| b == a + 1 && points[b].bucket_start - points[a].bucket_start <= TimeDelta::hours(36));
    let mut lines = Vec::new();
    let mut areas = Vec::new();
    let mut segments = Vec::new();
    for run in runs {
        let coords = run.iter().map(|&(i, rate)| format!("{:.2} {:.2}", px(i), py(rate))).collect::<Vec<_>>().join(" L");
        lines.push(format!("M{coords}"));
        if let [first, .., last] = run {
            areas.push(format!("M{:.2} {plot_bottom:.2} L{coords} L{:.2} {plot_bottom:.2} Z", px(first.0), px(last.0)));
        }
        segments.extend(run.windows(2).map(|pair| (pair[0], pair[1])));
    }
    let line_d = lines.join(" ");
    let active = hovered();
    let (readout, tooltip_spec) = match active {
        Some(CacheHover::Point(i)) => rates.get(i).copied().map(|rate| {
            let p = &points[i];
            let hit_pct = fmt_hit_pct(rate);
            let readout = format!(
                "{} · {} hit · {} read / {} in · {} write",
                p.label,
                hit_pct,
                fmt_count(p.cache_read_tokens),
                fmt_count(p.input_tokens),
                fmt_count(p.cache_write_tokens),
            );
            let spec = TooltipSpec {
                x: px(i),
                y: rate.map_or(plot_bottom, py),
                crosshair_x: Some(px(i)),
                y_top: plot_top,
                y_bottom: plot_bottom,
                view_w,
                title: format!("{} · {hit_pct} cache hit", p.label),
                detail: format!("{} read / {} input tokens", fmt_count(p.cache_read_tokens), fmt_count(p.input_tokens)),
                subdetail: Some(format!("{} write · {} req", fmt_count(p.cache_write_tokens), fmt_count(p.requests))),
            };
            (readout, spec)
        }),
        Some(CacheHover::Segment(i)) => segments.get(i).map(|&((a, from), (b, to))| {
            let delta = (to - from) * 100.0;
            let mx = (px(a) + px(b)) / 2.0;
            let my = (py(from) + py(to)) / 2.0;
            let title = format!("{} → {}", points[a].label, points[b].label);
            let detail = format!("{} → {} ({delta:+.1} pp)", fmt_hit_pct(Some(from)), fmt_hit_pct(Some(to)));
            let readout = format!("{title} · {detail}");
            let spec = TooltipSpec {
                x: mx,
                y: my,
                crosshair_x: Some(mx),
                y_top: plot_top,
                y_bottom: plot_bottom,
                view_w,
                title,
                detail,
                subdetail: Some(format!("Mean overall: {}", fmt_hit_pct(avg_rate))),
            };
            (readout, spec)
        }),
        None => None,
    }
    .unzip();
    rsx! {
        figure { class: "provider-chart chart-stage",
            div { class: "chart-header",
                figcaption { "Daily cache hit rate (%) · avg {fmt_hit_pct(avg_rate)}" }
                span { class: if readout.is_some() { "chart-readout active mono" } else { "chart-readout mono" },
                    {readout.as_deref().unwrap_or("Hover a line segment or point to inspect")}
                }
            }
            if points.is_empty() {
                p { class: "muted", "No cache observations in this range." }
            } else {
                svg {
                    view_box: "0 0 620 156",
                    role: "img",
                    "aria-label": "Daily cache hit rate percentage over time.",
                    onmouseleave: move |_| hovered.set(None),
                    defs {
                        linearGradient { id: "{grad_id}", x1: "0", y1: "0", x2: "0", y2: "1",
                            stop { offset: "0%", stop_color: "var(--ok)", stop_opacity: "0.28" }
                            stop { offset: "100%", stop_color: "var(--ok)", stop_opacity: "0.02" }
                        }
                    }
                    for pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
                        {
                            let gy = py(pct / 100.0);
                            rsx! {
                                line {
                                    x1: "{plot_left}",
                                    x2: "{plot_right}",
                                    y1: "{gy:.2}",
                                    y2: "{gy:.2}",
                                    class: if pct == 0.0 || pct == 100.0 { "chart-gridline" } else { "chart-gridline subtle" },
                                }
                                if pct == 0.0 || pct == 50.0 || pct == 100.0 {
                                    text { x: "{plot_left - 6.0}", y: "{gy + 3.5:.2}", text_anchor: "end", "{pct:.0}%" }
                                }
                            }
                        }
                    }
                    if let Some(avg_rate) = avg_rate {
                        line {
                            x1: "{plot_left}",
                            x2: "{plot_right}",
                            y1: "{py(avg_rate):.2}",
                            y2: "{py(avg_rate):.2}",
                            class: "chart-cache-avg",
                        }
                    }
                    for d in areas {
                        path { d: "{d}", fill: "{grad_url}", pointer_events: "none" }
                    }
                    path { class: "chart-line chart-cache-line", d: "{line_d}", pointer_events: "none" }
                    for (i, &((a, from), (b, to))) in segments.iter().enumerate() {
                        {
                            let x1 = px(a);
                            let y1 = py(from);
                            let x2 = px(b);
                            let y2 = py(to);
                            let is_seg = active == Some(CacheHover::Segment(i));
                            rsx! {
                                if is_seg {
                                    line { x1: "{x1:.2}", y1: "{y1:.2}", x2: "{x2:.2}", y2: "{y2:.2}", class: "chart-line chart-cache-line chart-segment-active", pointer_events: "none" }
                                }
                                line {
                                    x1: "{x1:.2}", y1: "{y1:.2}", x2: "{x2:.2}", y2: "{y2:.2}",
                                    class: "chart-hit",
                                    stroke: "transparent",
                                    stroke_width: "12",
                                    onmouseenter: move |_| hovered.set(Some(CacheHover::Segment(i))),
                                    onmouseleave: move |_| hovered.set(None),
                                }
                            }
                        }
                    }
                    for (i, &rate) in rates.iter().enumerate() {
                        {
                            let p = &points[i];
                            let cx = px(i);
                            let cy = rate.map_or(plot_bottom, py);
                            let is_pt = active == Some(CacheHover::Point(i));
                            rsx! {
                                if rate.is_some() {
                                    circle {
                                        cx: "{cx:.2}",
                                        cy: "{cy:.2}",
                                        r: if is_pt { "4.4" } else { "2.8" },
                                        class: if is_pt { "chart-cache-dot active" } else { "chart-cache-dot" },
                                        pointer_events: "none",
                                    }
                                }
                                circle {
                                    cx: "{cx:.2}",
                                    cy: "{cy:.2}",
                                    r: "8",
                                    fill: "transparent",
                                    class: "chart-hit",
                                    onmouseenter: move |_| hovered.set(Some(CacheHover::Point(i))),
                                    onmouseleave: move |_| hovered.set(None),
                                    title { "{p.label} · {fmt_hit_pct(rate)}" }
                                }
                            }
                        }
                    }
                    text { x: "{plot_left}", y: "149", "{points.first().map(|p| p.label.as_str()).unwrap_or_default()}" }
                    if n > 2 {
                        text { x: "{px(n / 2):.1}", y: "149", text_anchor: "middle", "{points[n / 2].label}" }
                    }
                    text { x: "{plot_right}", y: "149", text_anchor: "end", "{points.last().map(|p| p.label.as_str()).unwrap_or_default()}" }
                    if let Some(spec) = tooltip_spec {
                        SvgTooltip { spec }
                    }
                }
                details { summary { "Data table" }
                    Table { label: "Daily cache hit rate", headers: &["Day", "Cache hit %", "Cache read", "Input tokens"],
                        rows: points.iter().zip(&rates).map(|(p, rate)| vec![
                            p.label.clone(), fmt_hit_pct(*rate), fmt_count(p.cache_read_tokens), fmt_count(p.input_tokens),
                        ]).collect()
                    }
                }
            }
        }
    }
}

#[component]
pub(super) fn ActivityHeatmap(cells: Vec<HeatmapCell>, metric: Metric) -> Element {
    let mut hovered = use_signal(|| None::<(usize, usize)>);
    let unit = metric.label();
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let cell_map: BTreeMap<_, _> = cells
        .iter()
        .map(|c| {
            let rate = effective_cache_hit_rate(c.input_tokens, c.cache_read_tokens, c.cache_hit_rate);
            let value = metric.value(c.requests, c.tokens, c.api_equivalent_cost_usd, rate);
            ((usize::from(c.day_of_week), usize::from(c.hour)), (c, rate, value))
        })
        .collect();
    let max = if metric == Metric::CacheHitRate {
        100.0
    } else {
        cell_map.values().filter_map(|&(_, _, v)| v).fold(0.0, f64::max).max(f64::EPSILON)
    };
    let empty_value = metric.value(0, 0, 0.0, None);
    let value_at = |d, h| cell_map.get(&(d, h)).map_or(empty_value, |&(_, _, value)| value);
    let active = hovered();
    let cell_class = if metric == Metric::CacheHitRate { "chart-heat-cache" } else { "chart-bar chart-solid" };
    let readout = active.map(|(d, h)| {
        if let Some(&(c, rate, value)) = cell_map.get(&(d, h)) {
            format!(
                "{} {h:02}:00 · {} · {} req · {} tok · {} cache",
                days[d],
                metric.format_value(value),
                fmt_count(c.requests),
                fmt_count(c.tokens),
                fmt_hit_pct(rate),
            )
        } else {
            format!("{} {h:02}:00 · {} (no activity)", days[d], metric.format_value(empty_value))
        }
    });
    rsx! {
        figure { class: "provider-chart chart-stage",
            div { class: "chart-header",
                figcaption { "Activity by weekday and local hour · {unit}" }
                span { class: if readout.is_some() { "chart-readout active mono" } else { "chart-readout mono" },
                    {readout.as_deref().unwrap_or("Hover a cell to inspect")}
                }
            }
            div { class: "chart-legend",
                span { class: "muted", "Low" }
                for step in [0.14, 0.34, 0.56, 0.78, 1.0] {
                    span { class: if metric == Metric::CacheHitRate { "chart-heat-swatch cache" } else { "chart-heat-swatch" }, style: "opacity: {step}" }
                }
                span { class: "muted", "Peak ({fmt_compact_axis(metric, max)})" }
            }
            svg {
                view_box: "0 0 616 184",
                role: "img",
                "aria-label": "Seven by twenty-four activity heatmap. Data table follows.",
                onmouseleave: move |_| hovered.set(None),
                for hour in [0, 3, 6, 9, 12, 15, 18, 21, 23] {
                    text {
                        x: "{36 + 24 * hour}",
                        y: "12",
                        class: if active.is_some_and(|(_, h)| h == hour) { "chart-axis-active" } else { "" },
                        "{hour:02}"
                    }
                }
                for (d, day) in days.into_iter().enumerate() {
                    text {
                        x: "2",
                        y: "{34 + 22 * d}",
                        class: if active.is_some_and(|(ad, _)| ad == d) { "chart-axis-active" } else { "" },
                        "{day}"
                    }
                    for h in 0..24 {
                        {
                            let value = value_at(d, h);
                            let intensity = value.unwrap_or_default();
                            let is_active = active == Some((d, h));
                            let cx = 36 + 24 * h;
                            let cy = 20 + 22 * d;
                            rsx! {
                                rect {
                                    x: "{cx}",
                                    y: "{cy}",
                                    width: "21",
                                    height: "19",
                                    rx: "3",
                                    class: if is_active { "{cell_class} chart-cell-active chart-hit" } else { "{cell_class} chart-hit" },
                                    opacity: if intensity <= 0.0 { 0.12 } else { 0.24 + 0.76 * (intensity / max).clamp(0.0, 1.0) },
                                    onmouseenter: move |_| hovered.set(Some((d, h))),
                                    title { "{day} {h:02}:00 · {unit}: {metric.format_value(value)}" }
                                }
                            }
                        }
                    }
                }
                if let Some((d, h)) = active {
                    {
                        let (detail, subdetail) = if let Some(&(c, rate, _)) = cell_map.get(&(d, h)) {
                            (
                                format!("{} req · {} tok", fmt_count(c.requests), fmt_count(c.tokens)),
                                Some(format!("Cache {} · ${:.4}", fmt_hit_pct(rate), c.api_equivalent_cost_usd)),
                            )
                        } else {
                            ("No recorded requests".into(), None)
                        };
                        let spec = TooltipSpec {
                            x: 36.0 + 24.0 * h as f64 + 10.5,
                            y: 20.0 + 22.0 * d as f64 + 9.5,
                            crosshair_x: None,
                            y_top: 20.0,
                            y_bottom: 174.0,
                            view_w: 616.0,
                            title: format!("{} {h:02}:00 · {}", days[d], metric.format_value(value_at(d, h))),
                            detail,
                            subdetail,
                        };
                        rsx! { SvgTooltip { spec } }
                    }
                }
            }
            if cells.is_empty() { p { class: "muted", "No activity observations in this scope." } }
            details { summary { "Recorded cells data table" }
                Table { label: "Heatmap cells", headers: &["Day", "Hour", "Value", "Requests", "Tokens", "Cache hit %"],
                    rows: cell_map.iter().filter_map(|(&(d, h), &(c, rate, value))| {
                        Some(vec![
                            days.get(d)?.to_string(),
                            format!("{h:02}:00"),
                            metric.format_value(value),
                            fmt_count(c.requests),
                            fmt_count(c.tokens),
                            fmt_hit_pct(rate),
                        ])
                    }).collect()
                }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
enum DriftHover {
    Day(usize),
    Segment(usize),
}

#[component]
pub(super) fn DriftBandChart(days: Vec<DriftDayPoint>, markers: Vec<DriftMarker>) -> Element {
    let mut hovered = use_signal(|| None::<DriftHover>);
    let view_w = 620.0_f64;
    let plot_left = 48.0_f64;
    let plot_right = 604.0_f64;
    let plot_top = 16.0_f64;
    let plot_bottom = 140.0_f64;
    let max = days.iter().flat_map(|d| [d.thinking_per_reply, d.baseline_high.unwrap_or(0.0)]).fold(1.0, f64::max);
    let n = days.len();
    let dx = |i: usize| {
        if n <= 1 { (plot_left + plot_right) / 2.0 } else { plot_left + (plot_right - plot_left) * i as f64 / (n - 1) as f64 }
    };
    let dy = |v: f64| plot_bottom - (plot_bottom - plot_top) * (v / max).clamp(0.0, 1.0);
    let band_path = |range: &[(usize, &DriftDayPoint)]| {
        let high = range.iter().map(|(i, d)| format!("{:.2},{:.2}", dx(*i), dy(d.baseline_high.unwrap_or_default())));
        let low = range.iter().rev().map(|(i, d)| format!("{:.2},{:.2}", dx(*i), dy(d.baseline_low.unwrap_or_default())));
        high.chain(low).collect::<Vec<_>>().join(" ")
    };
    let trend_d = days
        .iter()
        .enumerate()
        .map(|(i, d)| format!("{}{:.2} {:.2}", if i == 0 { 'M' } else { 'L' }, dx(i), dy(d.thinking_per_reply)))
        .collect::<Vec<_>>()
        .join(" ");
    let indexed: Vec<_> = days.iter().enumerate().collect();
    let groups = indexed.split(|(_, d)| d.baseline_low.is_none() || d.baseline_high.is_none());
    let active = hovered();
    let readout = match active {
        Some(DriftHover::Day(i)) if i < n => {
            let d = &days[i];
            let band = match (d.baseline_low, d.baseline_median_thinking, d.baseline_high) {
                (Some(lo), Some(med), Some(hi)) => format!("normal {lo:.0}–{med:.0}–{hi:.0}"),
                _ => "baseline building".into(),
            };
            let tag = if d.outlier { " · OUTLIER" } else { "" };
            Some(format!("{} · {:.1} tok/reply ({} replies) · {band}{tag}", d.date, d.thinking_per_reply, d.replies))
        }
        Some(DriftHover::Segment(i)) if i + 1 < n => {
            let a = &days[i];
            let b = &days[i + 1];
            let delta = b.thinking_per_reply - a.thinking_per_reply;
            Some(format!("{} → {} · {:.1} → {:.1} tok/reply ({delta:+.1})", a.date, b.date, a.thinking_per_reply, b.thinking_per_reply))
        }
        _ => None,
    };
    let tooltip_spec = match active {
        Some(DriftHover::Day(i)) if i < n => {
            let d = &days[i];
            let marker = markers.iter().find(|m| m.date == d.date).map(|m| m.label.clone());
            let band = match (d.baseline_low, d.baseline_median_thinking, d.baseline_high) {
                (Some(lo), Some(med), Some(hi)) => format!("Normal: {lo:.0}–{med:.0}–{hi:.0}"),
                _ => "Insufficient baseline coverage".into(),
            };
            let sub = match (d.outlier, marker) {
                (true, Some(m)) => Some(format!("Outlier · {m}")),
                (true, None) => Some("Outlier vs 14-day baseline".into()),
                (false, Some(m)) => Some(format!("Change: {m}")),
                (false, None) => Some(band.clone()),
            };
            Some(TooltipSpec {
                x: dx(i),
                y: dy(d.thinking_per_reply),
                crosshair_x: Some(dx(i)),
                y_top: plot_top,
                y_bottom: plot_bottom,
                view_w,
                title: format!("{} · {:.1} tok/reply", d.date, d.thinking_per_reply),
                detail: format!("{} replies · {band}", d.replies),
                subdetail: sub.filter(|s| s != &band),
            })
        }
        Some(DriftHover::Segment(i)) if i + 1 < n => {
            let a = &days[i];
            let b = &days[i + 1];
            let delta = b.thinking_per_reply - a.thinking_per_reply;
            let mx = (dx(i) + dx(i + 1)) / 2.0;
            let my = (dy(a.thinking_per_reply) + dy(b.thinking_per_reply)) / 2.0;
            Some(TooltipSpec {
                x: mx,
                y: my,
                crosshair_x: Some(mx),
                y_top: plot_top,
                y_bottom: plot_bottom,
                view_w,
                title: format!("{} → {}", a.date, b.date),
                detail: format!("{:.1} → {:.1} tok/reply ({delta:+.1})", a.thinking_per_reply, b.thinking_per_reply),
                subdetail: Some(format!("{} → {} replies", a.replies, b.replies)),
            })
        }
        _ => None,
    };
    rsx! {
        figure { class: "provider-chart chart-stage",
            div { class: "chart-header",
                figcaption { "Thinking tokens per reply · preceding 14-day normal band and median" }
                span { class: if readout.is_some() { "chart-readout active mono" } else { "chart-readout mono" },
                    {readout.as_deref().unwrap_or("Hover a day or segment to inspect")}
                }
            }
            div { class: "chart-legend",
                span { class: "chart-legend-item", span { class: "chart-swatch" } "Observed" }
                span { class: "chart-legend-item", span { class: "chart-swatch band" } "14d normal band" }
                span { class: "chart-legend-item", span { class: "chart-swatch reset" } "Median / change" }
                span { class: "chart-legend-item", span { class: "chart-swatch outlier" } "Outlier" }
            }
            svg {
                view_box: "0 0 620 164",
                role: "img",
                "aria-label": "Daily thinking tokens, normal band, median, outliers and change markers. Daily data table follows.",
                onmouseleave: move |_| hovered.set(None),
                for frac in [0.0, 0.5, 1.0] {
                    {
                        let gy = plot_bottom - (plot_bottom - plot_top) * frac;
                        rsx! {
                            line {
                                x1: "{plot_left}",
                                x2: "{plot_right}",
                                y1: "{gy:.2}",
                                y2: "{gy:.2}",
                                class: if frac == 0.0 { "chart-gridline" } else { "chart-gridline subtle" },
                            }
                            text { x: "{plot_left - 6.0}", y: "{gy + 3.5:.2}", text_anchor: "end", "{max * frac:.0}" }
                        }
                    }
                }
                for group in groups.filter(|group| !group.is_empty()) {
                    polygon { class: "chart-band", points: band_path(group), pointer_events: "none" }
                }
                if n >= 2 {
                    path { class: "chart-line chart-drift-line", d: "{trend_d}", pointer_events: "none" }
                }
                for i in 0..n.saturating_sub(1) {
                    {
                        let x1 = dx(i);
                        let y1 = dy(days[i].thinking_per_reply);
                        let x2 = dx(i + 1);
                        let y2 = dy(days[i + 1].thinking_per_reply);
                        let is_seg = active == Some(DriftHover::Segment(i));
                        rsx! {
                            if is_seg {
                                line { x1: "{x1:.2}", y1: "{y1:.2}", x2: "{x2:.2}", y2: "{y2:.2}", class: "chart-line chart-drift-line chart-segment-active", pointer_events: "none" }
                            }
                            line {
                                x1: "{x1:.2}", y1: "{y1:.2}", x2: "{x2:.2}", y2: "{y2:.2}",
                                class: "chart-hit",
                                stroke: "transparent",
                                stroke_width: "12",
                                onmouseenter: move |_| hovered.set(Some(DriftHover::Segment(i))),
                            }
                        }
                    }
                }
                for (i, day) in days.iter().enumerate() {
                    {
                        let cx = dx(i);
                        let cy = dy(day.thinking_per_reply);
                        let is_day = active == Some(DriftHover::Day(i));
                        rsx! {
                            if let Some(marker) = markers.iter().find(|m| m.date == day.date) {
                                line { x1: "{cx:.2}", x2: "{cx:.2}", y1: "{plot_top}", y2: "{plot_bottom}", class: "chart-reset", title { "{marker.label}" } }
                            }
                            if let Some(median) = day.baseline_median_thinking {
                                line { x1: "{cx - 4.0:.2}", x2: "{cx + 4.0:.2}", y1: "{dy(median):.2}", y2: "{dy(median):.2}", class: "chart-median", pointer_events: "none" }
                            }
                            circle {
                                cx: "{cx:.2}",
                                cy: "{cy:.2}",
                                r: if is_day { "5" } else if day.outlier { "4.2" } else { "2.8" },
                                class: if day.outlier { "chart-outlier" } else { "chart-bar chart-solid" },
                                pointer_events: "none",
                            }
                            circle {
                                cx: "{cx:.2}",
                                cy: "{cy:.2}",
                                r: "8",
                                fill: "transparent",
                                class: "chart-hit",
                                onmouseenter: move |_| hovered.set(Some(DriftHover::Day(i))),
                                title { "{day.date} · {day.thinking_per_reply:.1} thinking tokens per reply" }
                            }
                        }
                    }
                }
                text { x: "{plot_left}", y: "156", "{days.first().map(|d| d.date.as_str()).unwrap_or_default()}" }
                if n > 2 {
                    text { x: "{(plot_left + plot_right) / 2.0:.1}", y: "156", text_anchor: "middle", "{days[n / 2].date}" }
                }
                text { x: "{plot_right}", y: "156", text_anchor: "end", "{days.last().map(|d| d.date.as_str()).unwrap_or_default()}" }
                if let Some(spec) = tooltip_spec {
                    SvgTooltip { spec }
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

    #[test]
    fn cache_hit_metric_parses_and_formats_percentages() {
        assert_eq!(Metric::parse("cache_hit"), Metric::CacheHitRate);
        assert_eq!(Metric::CacheHitRate.value(10, 500, 1.0, Some(0.753)), Some(75.3));
        assert_eq!(Metric::CacheHitRate.format_value(Some(75.3)), "75.3%");
        assert_eq!(request_cache_hit_rate(1000, 750), Some(0.75));
        assert_eq!(request_cache_hit_rate(0, 0), None);
        assert_eq!(fmt_hit_pct(Some(1.5)), "100.0%");
        assert_eq!(effective_cache_hit_rate(100, 0, 0.8), Some(0.0));
    }

    fn daily_point(day: i64, input_tokens: u64) -> ActivityTrendPoint {
        let bucket_start = DateTime::UNIX_EPOCH + TimeDelta::days(day);
        ActivityTrendPoint {
            bucket_start,
            label: bucket_start.date_naive().to_string(),
            requests: 1,
            failures: 0,
            input_tokens,
            output_tokens: 0,
            cache_read_tokens: 80,
            cache_write_tokens: 0,
            cache_hit_rate: 0.0,
            thinking_tokens: 0,
            api_equivalent_cost_usd: 0.0,
        }
    }

    #[test]
    fn daily_cache_rate_uses_counts_for_old_hubs() {
        fn app() -> Element {
            rsx! { CacheHitRateChart { points: vec![daily_point(0, 100)] } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("1970-01-01 · 80.0%"));
    }

    #[test]
    fn zero_input_day_breaks_the_daily_cache_path() {
        fn app() -> Element {
            rsx! { CacheHitRateChart { points: vec![daily_point(0, 100), daily_point(1, 0), daily_point(2, 100)] } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("M44.00 39.60 M604.00 39.60"));
        assert!(rendered.contains("1970-01-02 · —"));
    }

    #[test]
    fn missing_day_breaks_the_daily_cache_path_and_leaves_time_proportional_space() {
        fn app() -> Element {
            rsx! { CacheHitRateChart { points: vec![daily_point(0, 100), daily_point(1, 100), daily_point(3, 100), daily_point(4, 100)] } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("M44.00 39.60 L184.00 39.60 M464.00 39.60 L604.00 39.60"));
    }

    #[test]
    fn daily_activity_sorts_buckets_and_displays_missing_days() {
        fn app() -> Element {
            let mut points = vec![daily_point(2, 200), daily_point(0, 100)];
            for point in &mut points {
                point.label = point.bucket_start.format("%m-%d").to_string();
            }
            rsx! { ActivityTrendChart { points, metric: Metric::CacheHitRate } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        let first = rendered.find("01-01 · Cache hit rate (%): 80.0%").unwrap();
        let missing = rendered.find("01-02 · Cache hit rate (%): —").expect("missing day should have a slot");
        let last = rendered.find("01-03 · Cache hit rate (%): 40.0%").unwrap();
        assert!(first < missing && missing < last);
    }

    #[test]
    fn tooltip_box_clamps_within_svg_viewport() {
        let (left_x, top_y) = clamp_tooltip_box(5.0, 10.0, 160.0, 44.0, 620.0, 4.0);
        assert!(left_x >= 6.0);
        assert!(top_y >= 20.0, "near-top anchor should flip tooltip below");
        let (right_x, normal_y) = clamp_tooltip_box(615.0, 120.0, 160.0, 44.0, 620.0, 4.0);
        assert!(right_x + 160.0 <= 614.0);
        assert!(normal_y < 120.0, "lower anchor should place tooltip above");
    }

    #[test]
    fn uplifted_charts_render_with_hover_targets_and_cache_hit_rate() {
        fn app() -> Element {
            let now = Utc::now();
            let from = now - TimeDelta::hours(6);
            let samples = vec![
                QuotaSample {
                    id: 1,
                    provider_id: "google".into(),
                    account_id: "ag-a".into(),
                    window_key: "5h".into(),
                    label: "5h".into(),
                    kind: QuotaWindowKind::Session,
                    unit: QuotaUnit::Percent,
                    used_percent: Some(20.0),
                    remaining_percent: Some(80.0),
                    used_amount: None,
                    limit_amount: None,
                    resets_at: None,
                    window_duration_mins: Some(300),
                    cadence: ResetCadence::Fixed,
                    status: QuotaStatus::Allowed,
                    observation_state: QuotaObservationState::Measured,
                    observed_at: from + TimeDelta::hours(1),
                    recorded_at: from + TimeDelta::hours(1),
                    stale: false,
                    error_reason: None,
                },
                QuotaSample {
                    id: 2,
                    provider_id: "google".into(),
                    account_id: "ag-a".into(),
                    window_key: "5h".into(),
                    label: "5h".into(),
                    kind: QuotaWindowKind::Session,
                    unit: QuotaUnit::Percent,
                    used_percent: Some(45.0),
                    remaining_percent: Some(55.0),
                    used_amount: None,
                    limit_amount: None,
                    resets_at: None,
                    window_duration_mins: Some(300),
                    cadence: ResetCadence::Fixed,
                    status: QuotaStatus::Allowed,
                    observation_state: QuotaObservationState::Measured,
                    observed_at: from + TimeDelta::hours(3),
                    recorded_at: from + TimeDelta::hours(3),
                    stale: false,
                    error_reason: None,
                },
            ];
            let trend = vec![
                ActivityTrendPoint {
                    bucket_start: from,
                    label: "2026-11-22".into(),
                    requests: 12,
                    failures: 0,
                    input_tokens: 10_000,
                    output_tokens: 2_000,
                    cache_read_tokens: 7_500,
                    cache_write_tokens: 500,
                    cache_hit_rate: 0.75,
                    thinking_tokens: 300,
                    api_equivalent_cost_usd: 0.12,
                },
                ActivityTrendPoint {
                    bucket_start: now,
                    label: "2026-11-23".into(),
                    requests: 20,
                    failures: 1,
                    input_tokens: 20_000,
                    output_tokens: 4_000,
                    cache_read_tokens: 16_000,
                    cache_write_tokens: 1_000,
                    cache_hit_rate: 0.80,
                    thinking_tokens: 600,
                    api_equivalent_cost_usd: 0.24,
                },
            ];
            let cells = vec![HeatmapCell {
                day_of_week: 0,
                hour: 10,
                requests: 5,
                tokens: 5_000,
                input_tokens: 4_000,
                cache_read_tokens: 3_000,
                cache_hit_rate: 0.75,
                api_equivalent_cost_usd: 0.05,
            }];
            let drift_days = vec![
                DriftDayPoint {
                    date: "2026-11-22".into(),
                    replies: 40,
                    thinking_per_reply: 120.0,
                    output_per_reply: 300.0,
                    no_thinking_share: 0.0,
                    median_ttft_ms: Some(250.0),
                    failure_rate: 0.0,
                    baseline_median_thinking: Some(115.0),
                    baseline_low: Some(90.0),
                    baseline_high: Some(140.0),
                    judgeable: true,
                    outlier: false,
                },
                DriftDayPoint {
                    date: "2026-11-23".into(),
                    replies: 45,
                    thinking_per_reply: 180.0,
                    output_per_reply: 320.0,
                    no_thinking_share: 0.0,
                    median_ttft_ms: Some(260.0),
                    failure_rate: 0.0,
                    baseline_median_thinking: Some(118.0),
                    baseline_low: Some(90.0),
                    baseline_high: Some(145.0),
                    judgeable: true,
                    outlier: true,
                },
            ];
            rsx! {
                QuotaHistoryChart { samples, forecasts: vec![], resets: vec![], from, until: now, names: BTreeMap::from([("ag-a".into(), "Alpha".into())]) }
                CacheHitRateChart { points: trend.clone() }
                ActivityTrendChart { points: trend, metric: Metric::CacheHitRate }
                ActivityHeatmap { cells, metric: Metric::CacheHitRate }
                DriftBandChart { days: drift_days, markers: vec![] }
            }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("chart-stage"));
        assert!(rendered.contains("chart-readout"));
        assert!(rendered.contains("chart-hit"));
        assert!(rendered.contains("Daily cache hit rate (%)"));
        assert!(rendered.contains("Cache hit rate (%)"));
    }
}
