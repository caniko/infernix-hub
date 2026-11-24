//! SVGs use measured/estimated contract data and have equivalent text tables.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    rc::Rc,
};

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

pub(super) fn window_label(label: &str) -> &str {
    // Older fixture observations embed observation state in the window name.
    [" · measured zero", " · never observed", " · unavailable"].iter().find_map(|suffix| label.strip_suffix(suffix)).unwrap_or(label)
}

#[component]
pub(super) fn ResetTime(at: Option<DateTime<Utc>>) -> Element {
    let Some(at) = at else {
        return rsx! { "Reset not observed" };
    };
    let now = Utc::now();
    let minutes = (at - now).num_minutes();
    rsx! {
        if at <= now { "Reset was due" } else { "Resets in {minutes / 60}h {minutes % 60}m" }
        " · " span { class: "nowrap-unit", "{fmt_dt(at)}" }
    }
}

#[component]
pub(super) fn QuotaLimitBar(window: QuotaWindow, #[props(default)] forecast: Option<QuotaForecast>) -> Element {
    let (remaining, used) = quota_values(window.observation_state, window.remaining_percent, window.used_percent);
    let (label, tone) = freshness(&window);
    let status_label = quota_status_label(window.status);
    rsx! {
        div { class: "quota-window",
            div { class: "row",
                strong { "{window_label(&window.label)}" }
                if let Some(model) = &window.scope_model { span { class: "muted", "{model}" } }
                if window.observation_state == QuotaObservationState::NeverObserved {
                    span { class: "form-help", "{label}" }
                } else {
                    Chip { label: label.to_string(), tone: Some(tone.to_string()) }
                }
                if let Some(status) = status_label { span { class: "muted", "{status}" } }
            }
            if let (Some(left), Some(used)) = (remaining, used) {
                Meter { left, used, resets_at: window.resets_at, window_seconds: window.window_duration_mins.map(|mins| u64::from(mins) * 60),
                    forecast: forecast.filter(|_| !window.stale && window.observation_state == QuotaObservationState::Measured) }
            } else if matches!(window.observation_state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood) {
                p { class: "muted", "Percentage not observed" }
            }
            if matches!(window.observation_state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood)
                && let Some(amount) = window.used_amount {
                p { {format!("{} / {} {}", number(Some(amount)), number(window.limit_amount), window.currency.as_deref().unwrap_or_else(|| quota_unit_label(window.unit)))} }
            }
            p { class: "muted",
                ResetTime { at: window.resets_at }
                if let Some(cadence) = reset_cadence_label(window.cadence) { " · {cadence}" }
                if let Some(observed) = window.observed_at { " · observed " span { class: "nowrap-unit", "{fmt_dt(observed)}" } }
            }
            if let Some(reason) = &window.error_reason { p { class: "muted", "{reason}" } }
        }
    }
}

fn pace_tick(now: DateTime<Utc>, reset: Option<DateTime<Utc>>, seconds: Option<u64>) -> Option<f64> {
    let duration = seconds.filter(|s| *s > 0)? as f64;
    Some((1.0 - (reset? - now).as_seconds_f64() / duration).clamp(0.0, 1.0) * 100.0)
}

fn window_end_estimate(forecast: &QuotaForecast, left: f64, reset: DateTime<Utc>, now: DateTime<Utc>) -> Option<(f64, bool)> {
    let first = forecast.points.first()?;
    let rate = forecast.drain_pct_per_hour;
    // The pool's rate/anchor may describe a different cohort than the weighted reading.
    if reset <= now
        || first.at >= reset
        || !first.remaining_percent.is_finite()
        || (first.remaining_percent - left).abs() > 0.05
        || !rate.is_finite()
        || rate < 0.0
    {
        return None;
    }
    let end = (first.remaining_percent - rate * (reset - first.at).as_seconds_f64() / 3600.0).clamp(0.0, left.clamp(0.0, 100.0));
    let runs_out = forecast.projected_exhaustion_at.is_some_and(|at| at < reset);
    Some((end, runs_out))
}

#[component]
pub(super) fn Meter(
    left: f64,
    used: f64,
    resets_at: Option<DateTime<Utc>>,
    window_seconds: Option<u64>,
    forecast: Option<QuotaForecast>,
) -> Element {
    let hatch = use_hook(|| format!("quota-hatch-{}", Uuid::new_v4().simple()));
    let now = Utc::now();
    let pace = pace_tick(now, resets_at, window_seconds);
    let pace_label = pace.map(|pct| format!("Window pace: {pct:.0}% elapsed"));
    let estimate = forecast.as_ref().zip(resets_at).and_then(|(f, reset)| window_end_estimate(f, left, reset, now));
    let width = left.clamp(0.0, 100.0);
    let fill_class = if width < 10.0 {
        "quota-left bad"
    } else if width <= 30.0 {
        "quota-left warn"
    } else {
        "quota-left"
    };
    let used_caption = if used == 0.0 { "0% used".to_string() } else { format!("{used:.1}% used") };
    rsx! {
        div { class: "row",
            strong { class: "mono nowrap-unit", "{left:.1}% left" }
            span { class: "form-help nowrap-unit", "{used_caption}" }
        }
        svg { class: "quota-meter", view_box: "0 0 100 10", preserve_aspect_ratio: "none", "aria-hidden": "true",
            rect { y: "2", width: "100", height: "6", class: "quota-track" }
            rect { y: "2", width: "{width}", height: "6", class: "{fill_class}" }
            if let Some((end, risk)) = estimate {
                defs {
                    pattern { id: "{hatch}", width: "1.8", height: "6", pattern_units: "userSpaceOnUse",
                        path { d: "M-1.8 6L0 0 M0 6L1.8 0 M1.8 6L3.6 0",
                            class: if risk { "quota-hatch bad" } else { "quota-hatch" } }
                    }
                }
                rect { x: "{end}", y: "2", width: "{width - end}", height: "6", fill: "url(#{hatch})" }
            }
            if let (Some(pace), Some(label)) = (pace, &pace_label) {
                {
                    // The track runs from 0% left to 100% left, so pace is the remaining-time benchmark.
                    let x = (100.0 - pace).clamp(0.3, 99.7);
                    rsx! { line { x1: "{x}", x2: "{x}", y1: "0", y2: "10", class: "quota-pace", title { "{label}" } } }
                }
            }
        }
        if pace_label.is_some() || estimate.is_some() {
            div { class: "row quota-meter-meta",
                if let Some(label) = &pace_label { span { "{label}" } }
                if let Some((end, risk)) = estimate {
                    span { class: if risk { "quota-risk" } else { "" }, title: "Estimate at the reported reset, assuming the recent drain rate continues.",
                        if risk { "Runs out before reset" } else { "Ends ~{end:.0}% left" }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TimeBounds {
    pub(super) start: DateTime<Utc>,
    pub(super) end: DateTime<Utc>,
}

impl TimeBounds {
    pub(super) fn at(self, fraction: f64) -> DateTime<Utc> {
        self.start + TimeDelta::microseconds(((self.end - self.start).as_seconds_f64() * fraction * 1e6).round() as i64)
    }

    pub(super) fn fraction(self, at: DateTime<Utc>) -> f64 {
        (at - self.start).as_seconds_f64() / (self.end - self.start).as_seconds_f64().max(1e-6)
    }

    fn contains(self, at: DateTime<Utc>) -> bool {
        (self.start..=self.end).contains(&at)
    }

    fn fit(self, start: DateTime<Utc>, span: TimeDelta) -> Self {
        let start = start.clamp(self.start, self.end - span);
        Self { start, end: start + span }
    }

    pub(super) fn zoom(self, full: Self, anchor: f64, factor: f64) -> Self {
        let max = full.end - full.start;
        let anchor = anchor.clamp(0.0, 1.0);
        let span = TimeDelta::microseconds(((self.end - self.start).as_seconds_f64() * factor * 1e6).round() as i64)
            .clamp(TimeDelta::minutes(5).min(max), max);
        let offset = TimeDelta::microseconds((span.as_seconds_f64() * anchor * 1e6).round() as i64);
        full.fit(self.at(anchor) - offset, span)
    }

    pub(super) fn pan(self, full: Self, fraction: f64) -> Self {
        full.fit(self.at(fraction), (self.end - self.start).min(full.end - full.start))
    }

    fn timestamps(self, sorted: &[DateTime<Utc>]) -> &[DateTime<Utc>] {
        &sorted[sorted.partition_point(|t| *t < self.start)..sorted.partition_point(|t| *t <= self.end)]
    }
}

pub(super) fn nearest_timestamp(sorted: &[DateTime<Utc>], cursor: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let next = sorted.partition_point(|t| *t < cursor);
    match (next.checked_sub(1).and_then(|i| sorted.get(i)), sorted.get(next)) {
        (Some(&a), Some(&b)) => Some(if cursor - a <= b - cursor { a } else { b }),
        (a, b) => a.or(b).copied(),
    }
}

#[derive(Clone, Copy, PartialEq)]
struct TimeCursor {
    at: DateTime<Utc>,
    panel: usize,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct TimeScrub {
    view: Signal<Option<TimeBounds>>,
    cursor: Signal<Option<TimeCursor>>,
    drag: Signal<Option<(f64, TimeBounds)>>,
}

pub(super) fn use_time_scrub() -> TimeScrub {
    TimeScrub { view: use_signal(|| None), cursor: use_signal(|| None), drag: use_signal(|| None) }
}

impl TimeScrub {
    pub(super) fn bounds(self, full: TimeBounds) -> TimeBounds {
        self.view.read().map_or(full, |v| v.pan(full, 0.0))
    }

    pub(super) fn active(self) -> Option<DateTime<Utc>> {
        (*self.cursor.read()).map(|cursor| cursor.at)
    }

    pub(super) fn clear(mut self) {
        self.cursor.set(None);
        self.drag.set(None);
    }

    pub(super) fn reset(mut self) {
        self.view.set(None);
        self.clear();
    }

    pub(super) fn key(mut self, event: KeyboardEvent, full: TimeBounds, sorted: &[DateTime<Utc>]) {
        let view = self.bounds(full);
        match event.key() {
            Key::Named(key @ (NamedKey::ArrowLeft | NamedKey::ArrowRight)) => {
                let visible = view.timestamps(sorted);
                let previous = *self.cursor.peek();
                let idx = match (previous, key == NamedKey::ArrowRight) {
                    (Some(cursor), true) => visible.partition_point(|t| *t <= cursor.at).min(visible.len().saturating_sub(1)),
                    (Some(cursor), false) => visible.partition_point(|t| *t < cursor.at).saturating_sub(1),
                    (None, true) => 0,
                    (None, false) => visible.len().saturating_sub(1),
                };
                self.cursor.set(visible.get(idx).map(|&at| TimeCursor { at, panel: previous.map_or(0, |c| c.panel) }));
            }
            Key::Character(key) if matches!(key.as_str(), "+" | "=" | "-") => {
                let anchor = self.active().map_or(0.5, |at| view.fraction(at));
                self.view.set(Some(view.zoom(full, anchor, if key == "-" { 1.25 } else { 0.8 })));
            }
            Key::Character(key) if key == "0" => self.reset(),
            Key::Named(NamedKey::Escape) => self.reset(),
            _ => return,
        }
        event.prevent_default();
    }

    fn scrub(mut self, at: Option<DateTime<Utc>>, panel: usize) {
        let next = at.map(|at| TimeCursor { at, panel });
        let unchanged = *self.cursor.peek() == next;
        if !unchanged {
            self.cursor.set(next);
        }
    }
}

/// Pixel coordinates in a width-matched SVG viewBox (no letterboxing).
#[derive(Clone, Copy, PartialEq)]
pub(super) struct TimePlotBox {
    pub(super) left: f64,
    pub(super) right: f64,
    pub(super) top: f64,
    pub(super) bottom: f64,
}

impl TimePlotBox {
    pub(super) fn x(self, view: TimeBounds, at: DateTime<Utc>) -> f64 {
        self.left + (self.right - self.left) * view.fraction(at)
    }

    pub(super) fn axis_padding(self, view: TimeBounds) -> f64 {
        let day_line = if view.end - view.start > TimeDelta::hours(48) { 13.0 } else { 0.0 };
        let stagger = if self.right - self.left < 220.0 { 14.0 + day_line } else { 0.0 };
        33.0 + day_line + stagger
    }
}

#[component]
pub(super) fn TimeAxis(plot: TimePlotBox, view: TimeBounds) -> Element {
    let width = plot.right - plot.left;
    let ticks = if width < 340.0 {
        5
    } else if width < 600.0 || view.end - view.start < TimeDelta::minutes(6) {
        6
    } else {
        7
    };
    let multiday = view.end - view.start > TimeDelta::hours(48);
    rsx! {
        g { class: "time-axis", pointer_events: "none",
            for i in 0..ticks {
                {
                    let fraction = f64::from(i) / f64::from(ticks - 1);
                    let at = view.at(fraction);
                    let day_label = at.format("%b %d");
                    let clock_label = at.format("%H:%M");
                    let x = plot.left + width * fraction;
                    let stagger = if width < 220.0 && i % 2 == 1 { if multiday { 27.0 } else { 14.0 } } else { 0.0 };
                    let day_y = plot.bottom + 24.0 + stagger;
                    let time_y = day_y + if multiday { 13.0 } else { 0.0 };
                    let anchor = if i == 0 { "start" } else if i == ticks - 1 { "end" } else { "middle" };
                    rsx! {
                        line { x1: "{x}", x2: "{x}", y1: "{plot.top}", y2: "{plot.bottom}", class: "chart-gridline subtle time-gridline" }
                        if multiday {
                            text { x: "{x}", y: "{day_y}", text_anchor: anchor, "{day_label}" }
                        }
                        text { x: "{x}", y: "{time_y}", text_anchor: anchor, "{clock_label}" }
                    }
                }
            }
        }
    }
}

#[component]
pub(super) fn TimeCrosshair(plot: TimePlotBox, x: f64) -> Element {
    rsx! { line { x1: "{x}", x2: "{x}", y1: "{plot.top}", y2: "{plot.bottom + 8.0}", class: "chart-crosshair time-crosshair", pointer_events: "none" } }
}

#[component]
pub(super) fn TimePlotHit(
    plot: TimePlotBox,
    origin: Signal<(f64, f64)>,
    full: TimeBounds,
    sorted: Rc<Vec<DateTime<Utc>>>,
    mut scrub: TimeScrub,
    #[props(default)] panel: usize,
    #[props(default)] rail_times: Rc<Vec<DateTime<Utc>>>,
) -> Element {
    use dioxus::html::{
        geometry::WheelDelta,
        input_data::MouseButton,
        point_interaction::{InteractionLocation, PointerInteraction},
    };
    let zoomed = scrub.bounds(full) != full;
    let width = plot.right - plot.left;
    let fraction = move |client_x| (client_x - origin().0 - plot.left) / width;
    let hover_times = sorted.clone();
    rsx! {
        rect {
            x: "{plot.left}", y: "{plot.top}", width: "{width}", height: "{plot.bottom - plot.top + 10.0}", fill: "transparent",
            class: if scrub.drag.read().is_some() { "time-hit panning" } else if zoomed { "time-hit zoomed" } else { "time-hit" },
            onmousemove: move |e: MouseEvent| {
                let f = fraction(e.client_coordinates().x);
                let drag = *scrub.drag.peek();
                if let Some((start, initial)) = drag && e.held_buttons().contains(MouseButton::Primary) {
                    scrub.view.set(Some(initial.pan(full, start - f)));
                    return;
                }
                if drag.is_some() { scrub.drag.set(None); }
                let view = scrub.bounds(full);
                let at = view.at(f.clamp(0.0, 1.0));
                let near_rail = e.client_coordinates().y - origin().1 >= plot.bottom + 2.0;
                let event = near_rail.then(|| nearest_timestamp(view.timestamps(&rail_times), at))
                    .flatten().filter(|time| (view.fraction(*time) - f).abs() * width <= 6.0);
                scrub.scrub(event.or_else(|| nearest_timestamp(view.timestamps(&hover_times), at)), panel);
            },
            onmouseleave: move |_| scrub.clear(),
            onmousedown: move |e: MouseEvent| {
                let view = scrub.bounds(full);
                if view != full && e.trigger_button() == Some(MouseButton::Primary) {
                    let f = fraction(e.client_coordinates().x);
                    scrub.clear();
                    scrub.drag.set(Some((f, view)));
                }
            },
            onmouseup: move |_| { if scrub.drag.peek().is_some() { scrub.drag.set(None); } },
            onwheel: move |e: WheelEvent| {
                let delta = match e.delta() {
                    WheelDelta::Pixels(v) => v.y / 100.0,
                    WheelDelta::Lines(v) => v.y / 3.0,
                    WheelDelta::Pages(v) => v.y,
                };
                if delta == 0.0 { return; }
                e.prevent_default();
                let f = fraction(e.client_coordinates().x);
                let view = scrub.bounds(full).zoom(full, f, 1.25_f64.powf(delta.clamp(-3.0, 3.0)));
                scrub.drag.set(None);
                scrub.view.set(Some(view));
                scrub.scrub(nearest_timestamp(view.timestamps(&sorted), view.at(f.clamp(0.0, 1.0))), panel);
            },
            ondoubleclick: move |_| scrub.reset(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Reading {
    at: DateTime<Utc>,
    left: f64,
    reset: Option<DateTime<Utc>>,
}

fn same_quota_run(previous: &Reading, next: &Reading) -> bool {
    next.at - previous.at <= TimeDelta::hours(4)
        && !previous.reset.zip(next.reset).is_some_and(|(a, b)| b > a)
        && next.left - previous.left <= 20.0
}

fn measured_at(history: &[Reading], at: DateTime<Utc>) -> Option<Reading> {
    let next = history.partition_point(|p| p.at <= at);
    let point = *history.get(next.checked_sub(1)?)?;
    (point.at == at || history.get(next).is_some_and(|later| same_quota_run(&point, later))).then_some(point)
}

pub(super) type QuotaSeriesKey = (String, Option<String>);

#[derive(Clone, PartialEq)]
struct QuotaSeries {
    key: QuotaSeriesKey,
    label: String,
    history: Vec<Reading>,
    forecast: Vec<Reading>,
    exhaustion: Option<DateTime<Utc>>,
    next_reset: Option<DateTime<Utc>>,
    latest: Option<Reading>,
    resets: BTreeSet<DateTime<Utc>>,
}

impl QuotaSeries {
    fn color(index: usize, focused: bool) -> &'static str {
        const PALETTE: [&str; 8] = ["#8cafb0", "#98b27a", "#b0a1cb", "#b7a183", "#81a5cc", "#c696ac", "#90b69d", "#c0bb87"];
        if focused { "var(--honey)" } else { PALETTE[index % PALETTE.len()] }
    }

    fn at(&self, at: DateTime<Utc>, future: bool) -> Option<Reading> {
        if !future {
            return measured_at(&self.history, at);
        }
        let next = self.forecast.partition_point(|p| p.at < at);
        let to = *self.forecast.get(next)?;
        if to.at == at {
            return Some(to);
        }
        let from = self.forecast.get(next.checked_sub(1)?)?;
        if !same_forecast_run(from, &to) {
            return None;
        }
        let time = (at - from.at).as_seconds_f64() / (to.at - from.at).as_seconds_f64();
        Some(Reading { at, left: from.left + (to.left - from.left) * time, reset: to.reset })
    }
}

fn same_forecast_run(a: &Reading, b: &Reading) -> bool {
    a.reset == b.reset && b.left - a.left <= 20.0
}

fn quota_series(
    samples: &[QuotaSample],
    forecasts: &[QuotaForecast],
    names: &BTreeMap<String, String>,
    history: TimeBounds,
) -> Vec<QuotaSeries> {
    let mut groups = BTreeMap::<QuotaSeriesKey, Vec<Reading>>::new();
    for row in samples {
        if history.contains(row.observed_at)
            && !row.stale
            && row.observation_state == QuotaObservationState::Measured
            && let (Some(left), _) = quota_values(row.observation_state, row.remaining_percent, row.used_percent)
        {
            groups.entry((row.account_id.clone(), row.scope_model.clone())).or_default().push(Reading {
                at: row.observed_at,
                left,
                reset: row.resets_at,
            });
        }
    }
    for forecast in forecasts {
        if let Some(account) = &forecast.account_id {
            groups.entry((account.clone(), forecast.scope_model.clone())).or_default();
        }
    }
    let show_model = groups.keys().map(|(_, model)| model).collect::<BTreeSet<_>>().len() > 1;
    let mut series: Vec<_> = groups
        .into_iter()
        .map(|(key, mut points)| {
            points.sort_by_key(|p| p.at);
            let (account, model) = &key;
            let name = names.get(account).unwrap_or(account);
            let label = if show_model { format!("{name} · {}", model.as_deref().unwrap_or("Unscoped")) } else { name.clone() };
            let reset = points.last().and_then(|p| p.reset);
            QuotaSeries {
                key,
                label,
                next_reset: reset.filter(|at| *at > history.end),
                latest: points.last().copied(),
                resets: points.iter().filter_map(|p| p.reset).collect(),
                history: points,
                forecast: Vec::new(),
                exhaustion: None,
            }
        })
        .collect();
    series.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.key.cmp(&b.key)));
    series
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct QuotaChartScope {
    pub(super) history: TimeBounds,
    pub(super) forecast_until: Option<DateTime<Utc>>,
    pub(super) history_truncated: bool,
    pub(super) pool: bool,
}

#[component]
pub(super) fn QuotaHistoryChart(
    samples: Vec<QuotaSample>,
    forecasts: Vec<QuotaForecast>,
    names: BTreeMap<String, String>,
    scope: QuotaChartScope,
    #[props(default)] current: Vec<ModelAccount>,
    #[props(default)] window_key: String,
) -> Element {
    let as_of = scope.history.end;
    let full = TimeBounds { start: scope.history.start, end: scope.forecast_until.unwrap_or(as_of) };
    let mut series = quota_series(&samples, &forecasts, &names, scope.history);
    for series in &mut series {
        if let Some(window) = current
            .iter()
            .find(|a| a.id == series.key.0)
            .and_then(|a| a.windows.iter().find(|w| w.window_key == window_key && w.scope_model == series.key.1))
            && let Some(at) = window.observed_at.filter(|at| *at <= as_of)
            && let Some(left) = quota_values(window.observation_state, window.remaining_percent, window.used_percent).0
            && series.latest.is_none_or(|r| r.at <= at)
        {
            series.latest = Some(Reading { at, left, reset: window.resets_at });
            series.next_reset = window.resets_at.filter(|at| *at > as_of);
            series.resets.extend(window.resets_at);
        }
        if let Some(f) = forecasts.iter().find(|f| f.account_id.as_ref() == Some(&series.key.0) && f.scope_model == series.key.1) {
            series.exhaustion = f.projected_exhaustion_at.filter(|at| series.next_reset.is_some_and(|reset| *at < reset));
            series.forecast = f
                .points
                .iter()
                .filter(|p| p.at >= as_of && p.remaining_percent.is_finite())
                .map(|p| Reading { at: p.at, left: p.remaining_percent, reset: series.next_reset.filter(|reset| p.at >= *reset) })
                .collect();
            series.forecast.sort_by_key(|p| p.at);
        }
    }
    series.retain(|s| s.latest.is_some() || !s.history.is_empty() || (scope.forecast_until.is_some() && !s.forecast.is_empty()));
    let times = series
        .iter()
        .flat_map(|s| {
            s.history
                .iter()
                .map(|r| r.at)
                .chain(s.forecast.iter().filter(|r| r.at > as_of).map(|r| r.at))
                .chain(s.resets.iter().copied())
                .chain(s.exhaustion)
        })
        .filter(|at| full.contains(*at))
        .collect::<BTreeSet<_>>();
    let sorted = Rc::new(times.into_iter().collect());
    let series = Rc::new(series);
    rsx! {
        figure { class: "provider-chart chart-stage quota-history",
            figcaption { "Remaining quota · UTC" }
            if scope.history_truncated { Chip { label: "Showing newest 4,000 samples".to_string(), tone: Some("amber".to_string()) } }
            if !series.iter().any(|s| !s.history.is_empty()) {
                p { class: "form-help", "No measured percentage samples for this window and time range." }
            }
            QuotaExplorer { series: series.clone(), sorted, full, as_of, pool: scope.pool }
            QuotaHistoryTables { series, show_forecast: scope.forecast_until.is_some() }
        }
    }
}

#[component]
fn QuotaExplorer(
    series: Rc<Vec<QuotaSeries>>,
    sorted: Rc<Vec<DateTime<Utc>>>,
    full: TimeBounds,
    as_of: DateTime<Utc>,
    pool: bool,
) -> Element {
    let scrub = use_time_scrub();
    let mut focus = use_signal(|| None::<QuotaSeriesKey>);
    let mut multiples = use_signal(|| series.len() >= 10);
    let future = full.end > as_of;
    use_effect(use_reactive((&future,), move |_| scrub.reset()));
    let focus_key = focus.read();
    let selected = series.iter().position(|s| focus_key.as_ref() == Some(&s.key));
    let view = scrub.bounds(full);
    let has_layout_choice = pool && series.len() >= 4;
    let small = has_layout_choice && multiples();
    let summary = selected.or_else(|| (series.len() == 1).then_some(0));
    let keyboard_times = sorted.clone();
    if series.is_empty() {
        return rsx! {};
    }
    rsx! {
        if let Some(s) = summary.and_then(|i| series.get(i)) {
            div { class: "row quota-decision",
                if let Some(r) = s.latest {
                    strong { class: "mono", "{r.left:.1}% left" }
                } else {
                    strong { "Not observed" }
                }
                span { "Next reset: {date(s.next_reset)}" }
                span { "Last observed: {date(s.latest.map(|r| r.at))}" }
                if s.exhaustion.is_some() { span { class: "quota-risk", "May run out before reset" } }
            }
        } else {
            div { class: "row quota-decision",
                span { "{series.len()} separate quota traces · select a series for its latest reading and next reset" }
                if series.iter().any(|s| s.exhaustion.is_some()) { span { class: "quota-risk", "May run out before reset" } }
            }
        }
        div { class: "filter-row quota-series-chips", role: "group", aria_label: "Focus quota series",
            if series.len() > 1 {
                button { class: "filter-chip", aria_pressed: selected.is_none(), onclick: move |_| focus.set(None), "All" }
            }
            for (i, s) in series.iter().enumerate() {
                {
                    let key = s.key.clone();
                    let active = selected == Some(i);
                    let color = QuotaSeries::color(i, active);
                    rsx! {
                        button { key: "{i}", class: "filter-chip", aria_pressed: active,
                            style: "--chart-accent: {color}", onclick: move |_| focus.set(if active { None } else { Some(key.clone()) }),
                            span { class: "chart-swatch", aria_hidden: "true" } "{s.label}"
                        }
                    }
                }
            }
        }
        div { class: "row quota-plot-controls",
            if has_layout_choice {
                div { class: "segmented", role: "group", aria_label: "Quota chart layout",
                    button { class: "seg-btn", aria_pressed: !small, onclick: move |_| { multiples.set(false); scrub.clear(); }, "Overlay" }
                    button { class: "seg-btn", aria_pressed: small, onclick: move |_| { multiples.set(true); scrub.clear(); }, "Small multiples" }
                }
            }
            if view != full {
                button { class: "filter-chip", onclick: move |_| scrub.reset(), "Reset zoom" }
            }
            if future {
                span { class: "chart-legend-item", span { class: "chart-swatch dashed" } "Forecast (estimate)" }
            }
            span { class: "form-help", "Hover or focus chart and use ←/→ · wheel or +/− to zoom · drag to pan · 0 to reset" }
        }
        div { class: if small { "quota-plots multiples" } else { "quota-plots" },
            tabindex: "0", role: "group", aria_label: "Quota timeline. Arrow keys scrub, plus or minus zooms, zero or Escape resets.",
            onkeydown: move |e| scrub.key(e, full, &keyboard_times),
            onblur: move |_| scrub.clear(),
            if small {
                for (i, s) in series.iter().enumerate() {
                    QuotaPlot { key: "{i}", series: series.clone(), sorted: sorted.clone(), full, as_of, scrub, selected, single: Some(i), label: s.label.clone() }
                }
            } else {
                QuotaPlot { series, sorted, full, as_of, scrub, selected, single: None, label: "Remaining quota".to_string() }
            }
        }
    }
}

/// Measure the CSS box; resize updates keep the viewBox and hit target in CSS pixels.
#[derive(Clone, Copy)]
pub(super) struct TimePlotMeasure {
    mounted: Signal<Option<Rc<MountedData>>>,
    pub(super) width: Signal<f64>,
    pub(super) origin: Signal<(f64, f64)>,
}

pub(super) fn use_time_plot_measure() -> TimePlotMeasure {
    TimePlotMeasure { mounted: use_signal(|| None), width: use_signal(|| 620.0), origin: use_signal(|| (0.0, 0.0)) }
}

impl TimePlotMeasure {
    pub(super) async fn mount(mut self, event: MountedEvent) {
        self.mounted.set(Some(event.data()));
        self.refresh().await;
    }

    pub(super) async fn refresh(mut self) {
        let mounted = self.mounted.peek().clone();
        if let Some(el) = mounted
            && let Ok(rect) = el.get_client_rect().await
        {
            if rect.size.width > 0.0 && *self.width.peek() != rect.size.width {
                self.width.set(rect.size.width);
            }
            let origin = (rect.origin.x, rect.origin.y);
            if *self.origin.peek() != origin {
                self.origin.set(origin);
            }
        }
    }
}

fn quota_y(plot: TimePlotBox, pct: f64) -> f64 {
    plot.bottom - (plot.bottom - plot.top) * pct.clamp(0.0, 100.0) / 100.0
}

#[component]
fn QuotaTraces(
    series: Rc<Vec<QuotaSeries>>,
    plot: TimePlotBox,
    view: TimeBounds,
    as_of: DateTime<Utc>,
    selected: Option<usize>,
    single: Option<usize>,
) -> Element {
    let show_future = view.end > as_of;
    let rail = plot.bottom + 7.0;
    // Paint the highlighted trace last so a context line cannot cover it.
    let indices = (0..series.len()).filter(|i| Some(*i) != selected).chain(selected);
    rsx! {
        for i in indices.filter(|i| single.is_none_or(|j| j == *i)) {
            {
                let s = &series[i];
                let focused = selected == Some(i);
                let color = QuotaSeries::color(i, focused);
                let mut path = String::new();
                let mut projected = String::new();
                for run in s.history.chunk_by(same_quota_run).filter(|run| run.len() > 1) {
                    let first = run[0];
                    let _ = write!(path, "M{:.2} {:.2}", plot.x(view, first.at), quota_y(plot, first.left));
                    for point in &run[1..] {
                        let _ = write!(path, "H{:.2}V{:.2}", plot.x(view, point.at), quota_y(plot, point.left));
                    }
                }
                if show_future {
                    for run in s.forecast.chunk_by(same_forecast_run).filter(|run| run.len() > 1) {
                        for (i, point) in run.iter().enumerate() {
                            let _ = write!(projected, "{}{:.2} {:.2}", if i == 0 { 'M' } else { 'L' }, plot.x(view, point.at), quota_y(plot, point.left));
                        }
                    }
                }
                rsx! {
                    g { key: "{i}", class: if focused { "quota-trace focused" } else { "quota-trace" }, style: "color: {color}",
                        opacity: if selected.is_some() && !focused { 0.22 } else { 1.0 },
                        path { class: "chart-line", d: "{path}" }
                        for run in s.history.chunk_by(same_quota_run).filter(|run| run.len() == 1 && view.contains(run[0].at)) {
                            circle { cx: "{plot.x(view, run[0].at)}", cy: "{quota_y(plot, run[0].left)}", r: "2.4", class: "chart-point" }
                        }
                        if show_future {
                            path { class: "chart-line chart-projection", d: "{projected}" }
                            for run in s.forecast.chunk_by(same_forecast_run).filter(|run| run.len() == 1 && view.contains(run[0].at)) {
                                circle { cx: "{plot.x(view, run[0].at)}", cy: "{quota_y(plot, run[0].left)}", r: "2.4", class: "chart-point quota-forecast-point" }
                            }
                            if let Some(at) = s.exhaustion.filter(|at| view.contains(*at)) {
                                line { class: "chart-exhaustion", x1: "{plot.x(view, at)}", x2: "{plot.x(view, at)}", y1: "{plot.bottom - 9.0}", y2: "{rail}" }
                            }
                        }
                        for &at in s.resets.iter().filter(|&&at| view.contains(at)) {
                            {
                                let x = plot.x(view, at);
                                rsx! { path { class: "quota-reset-mark", d: "M{x} {rail - 3.0}l3 3-3 3-3-3z" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn QuotaPlot(
    series: Rc<Vec<QuotaSeries>>,
    sorted: Rc<Vec<DateTime<Utc>>>,
    full: TimeBounds,
    as_of: DateTime<Utc>,
    scrub: TimeScrub,
    selected: Option<usize>,
    single: Option<usize>,
    label: String,
) -> Element {
    let measure = use_time_plot_measure();
    let clip_id = use_hook(|| format!("quota-clip-{}", Uuid::new_v4().simple()));
    let width = (measure.width)().max(100.0);
    let plot = TimePlotBox { left: 38.0, right: width - 8.0, top: 22.0, bottom: if single.is_some() { 94.0 } else { 158.0 } };
    let view = scrub.bounds(full);
    let height = plot.bottom + plot.axis_padding(full);
    let active = scrub.active().filter(|at| view.contains(*at));
    let active_x = active.map(|at| plot.x(view, at));
    let panel = single.unwrap_or(0);
    let tooltip = (*scrub.cursor.read()).filter(|c| c.panel == panel && view.contains(c.at));
    let show_future = view.end > as_of;
    let now_x = plot.x(view, as_of).max(plot.left);
    let badge_x = now_x.clamp(plot.left + 16.0, plot.right - 16.0);
    let rail = plot.bottom + 7.0;
    let rail_times = Rc::new(
        series
            .iter()
            .enumerate()
            .filter(|(i, _)| single.is_none_or(|j| *i == j))
            .flat_map(|(_, s)| s.resets.iter().copied())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    );
    rsx! {
        div { class: "quota-plot",
            if single.is_some() { strong { class: "quota-plot-label", "{label}" } }
            div { class: "quota-plot-surface",
                onmounted: move |e| measure.mount(e),
                onresize: move |_| measure.refresh(),
                onmouseenter: move |_| measure.refresh(),
                svg { role: "img", "aria-label": "{label}. Remaining quota, 0 to 100 percent. Data tables follow.",
                    width: "100%", height: "{height}", view_box: "0 0 {width} {height}", preserve_aspect_ratio: "none",
                    defs { clipPath { id: "{clip_id}", rect { x: "{plot.left}", y: "{plot.top - 3.0}",
                        width: "{plot.right - plot.left}", height: "{rail - plot.top + 6.0}" } } }
                    if show_future {
                        rect { x: "{now_x}", y: "{plot.top}", width: "{plot.right - now_x}", height: "{plot.bottom - plot.top}", class: "quota-future" }
                    }
                    TimeAxis { plot, view }
                    for pct in [0.0, 25.0, 50.0, 75.0, 100.0] {
                        {
                            let y = quota_y(plot, pct);
                            rsx! {
                                line { x1: "{plot.left}", x2: "{plot.right}", y1: "{y}", y2: "{y}",
                                    class: if pct == 0.0 { "chart-gridline" } else { "chart-gridline subtle" } }
                                if pct % 50.0 == 0.0 {
                                    text { x: "{plot.left - 5.0}", y: "{y + 3.5}", text_anchor: "end", "{pct:.0}%" }
                                }
                            }
                        }
                    }
                    line { x1: "{plot.left}", x2: "{plot.right}", y1: "{rail}", y2: "{rail}", class: "chart-gridline" }
                    g { clip_path: "url(#{clip_id})", pointer_events: "none",
                        QuotaTraces { series: series.clone(), plot, view, as_of, selected, single }
                    }
                    if show_future && view.contains(as_of) {
                        line { x1: "{now_x}", x2: "{now_x}", y1: "18", y2: "{plot.bottom}", class: "quota-now-line" }
                        rect { x: "{badge_x - 16.0}", y: "2", width: "32", height: "16", rx: "4", class: "quota-now-badge" }
                        text { x: "{badge_x}", y: "14", text_anchor: "middle", "Now" }
                    }
                    if let (Some(at), Some(x)) = (active, active_x) {
                        TimeCrosshair { plot, x }
                        for (i, s) in series.iter().enumerate().filter(|(i, _)| single.is_none_or(|j| j == *i)) {
                            if let Some(r) = s.at(at, at > as_of) {
                                circle { key: "{i}", cx: "{x}", cy: "{quota_y(plot, r.left)}", r: "3.6", pointer_events: "none",
                                    class: if at > as_of { "chart-point active quota-forecast-point" } else { "chart-point active" },
                                    style: "color: {QuotaSeries::color(i, selected == Some(i))}" }
                            }
                        }
                    }
                    TimePlotHit { plot, origin: measure.origin, full, sorted, scrub, panel, rail_times }
                }
                div { role: "status",
                    if let Some(cursor) = tooltip {
                        QuotaReadout { series, at: cursor.at, as_of, selected, right: view.fraction(cursor.at) < 0.5 }
                    }
                }
            }
        }
    }
}

#[component]
fn QuotaReadout(series: Rc<Vec<QuotaSeries>>, at: DateTime<Utc>, as_of: DateTime<Utc>, selected: Option<usize>, right: bool) -> Element {
    let future = at > as_of;
    let timestamp = at.format("%Y-%m-%d %H:%M:%S UTC");
    let side = if right { "right" } else { "" };
    let density = if series.len() > 10 { "dense" } else { "" };
    rsx! {
        div { class: "quota-readout {side} {density}",
            strong { class: "mono", "{timestamp}" if future { " · Estimated" } }
            ul {
                for (i, s) in series.iter().enumerate() {
                    {
                        let r = s.at(at, future);
                        let color = QuotaSeries::color(i, selected == Some(i));
                        rsx! {
                            li { key: "{i}", style: "--chart-accent: {color}",
                                span { class: "chart-swatch", aria_hidden: "true" }
                                span { "{s.label}" }
                                span { class: "mono",
                                    if let Some(r) = r { "{r.left:.1}% left" } else { "—" }
                                }
                                if let Some(r) = r.filter(|r| r.at != at) {
                                    small { {format!("Held from {}", r.at.format("%H:%M:%S UTC"))} }
                                }
                                if s.resets.contains(&at) {
                                    small { if future { "Scheduled reset" } else { "Reported reset" } }
                                }
                                if s.exhaustion == Some(at) { small { class: "quota-risk", "Projected exhaustion" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn QuotaHistoryTables(series: Rc<Vec<QuotaSeries>>, show_forecast: bool) -> Element {
    rsx! {
        if show_forecast {
            details {
                summary { "Estimated projection data table" }
                Table { label: "Estimated projection", headers: &["Series", "Projected time", "Remaining %"],
                    rows: series.iter().flat_map(|s| s.forecast.iter().map(|r| vec![s.label.clone(), fmt_dt(r.at), number(Some(r.left))])).collect() }
            }
        }
        details {
            summary { "Measured history data table" }
            Table { label: "Measured quota", headers: &["Series", "Observed", "Remaining %", "Reported reset"],
                rows: series.iter().flat_map(|s| s.history.iter().map(|r| vec![s.label.clone(), fmt_dt(r.at), number(Some(r.left)), date(r.reset)])).collect() }
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

pub(super) const NO_RANGE_DATA: &str = "No data for selected range";

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
        if items.is_empty() {
            p { class: "form-help", "{NO_RANGE_DATA}" }
        } else {
            figure { class: "provider-chart chart-stage",
                div { class: "chart-header",
                    figcaption { "{label} · {unit}" }
                    span { class: if readout.is_some() { "chart-readout active mono" } else { "chart-readout mono" },
                        {readout.as_deref().unwrap_or("Hover a bar to inspect")}
                    }
                }
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
        if plotted.is_empty() {
            p { class: "form-help", "{NO_RANGE_DATA}" }
        } else {
            figure { class: "provider-chart chart-stage",
                div { class: "chart-header",
                    figcaption { "Daily cache hit rate (%) · avg {fmt_hit_pct(avg_rate)}" }
                    span { class: if readout.is_some() { "chart-readout active mono" } else { "chart-readout mono" },
                        {readout.as_deref().unwrap_or("Hover a line segment or point to inspect")}
                    }
                }
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
    if cells.is_empty() {
        return rsx! { p { class: "form-help", "{NO_RANGE_DATA}" } };
    }
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
    fn quota_bar_distinguishes_zero_usage_from_no_observation() {
        fn app() -> Element {
            let daily: QuotaWindow = serde_json::from_value(serde_json::json!({
                "window_key": "daily", "label": "Daily · measured zero", "used_percent": 0.0,
                "observation_state": "measured"
            }))
            .unwrap();
            let monthly: QuotaWindow = serde_json::from_value(serde_json::json!({
                "window_key": "monthly", "label": "Monthly · never observed"
            }))
            .unwrap();
            rsx! { QuotaLimitBar { window: daily } QuotaLimitBar { window: monthly } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.find("100.0% left").unwrap() < rendered.find("0% used").unwrap());
        assert!(rendered.contains("\"0% used\""));
        assert!(rendered.contains("Not observed"));
        assert!(!rendered.contains("measured zero"));
        assert!(!rendered.contains("never observed"));
    }

    #[test]
    fn measured_steps_break_at_gaps_resets_and_sharp_refills() {
        let start = DateTime::UNIX_EPOCH;
        let at = |h| start + TimeDelta::hours(h);
        let mk = |h, left, reset| Reading { at: at(h), left, reset: Some(at(reset)) };
        let points = [mk(0, 90.0, 5), mk(4, 80.0, 5), mk(5, 89.0, 10), mk(6, 30.0, 10), mk(7, 95.0, 10), mk(12, 92.0, 10)];
        assert_eq!(points.chunk_by(same_quota_run).map(<[_]>::len).collect::<Vec<_>>(), [2, 2, 1, 1]);
        assert_eq!(measured_at(&points, at(2)), Some(points[0]));
        assert_eq!(measured_at(&points, at(9)), None);
    }

    #[test]
    fn scope_models_get_separate_series_even_with_the_same_account_and_window() {
        let at = DateTime::UNIX_EPOCH;
        let sample = |model: &str| -> QuotaSample {
            serde_json::from_value(serde_json::json!({
                "id": 1, "provider_id": "example", "account_id": "a", "window_key": "weekly",
                "scope_model": model, "label": "Weekly", "kind": "weekly", "unit": "percent", "remaining_percent": 75.0,
                "cadence": "unknown", "status": "unknown", "observation_state": "measured", "observed_at": at,
                "recorded_at": at, "stale": false
            }))
            .unwrap()
        };
        let groups = quota_series(&[sample("opus"), sample("sonnet")], &[], &BTreeMap::new(), TimeBounds { start: at, end: at });
        assert_eq!(groups.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), ["a · opus", "a · sonnet"]);
        assert!(groups.iter().all(|s| s.history.len() == 1));
    }

    #[test]
    fn zoom_preserves_cursor_time_and_pan_stays_inside_full_bounds() {
        let start = DateTime::UNIX_EPOCH;
        let at = |h| start + TimeDelta::hours(h);
        let full = TimeBounds { start, end: at(24) };
        let zoom = full.zoom(full, 0.25, 0.5);
        assert_eq!(zoom.at(0.25), at(6));
        assert_eq!(zoom.pan(full, -2.0), TimeBounds { start, end: at(12) });
        assert_eq!(zoom.pan(full, 2.0), TimeBounds { start: at(12), end: at(24) });
        assert_eq!(zoom.zoom(full, 0.25, 10.0), full);
        assert_eq!(zoom.zoom(full, 0.25, 0.0).end - zoom.zoom(full, 0.25, 0.0).start, TimeDelta::minutes(5));
    }

    #[test]
    fn pace_is_elapsed_window_fraction_and_rejects_unknown_duration() {
        let now = DateTime::UNIX_EPOCH;
        let reset = |hours| Some(now + TimeDelta::hours(hours));
        assert_eq!(pace_tick(now, reset(2), Some(14_400)), Some(50.0));
        assert_eq!(pace_tick(now, reset(8), Some(14_400)), Some(0.0));
        assert_eq!(pace_tick(now, reset(-1), Some(14_400)), Some(100.0));
        assert_eq!(pace_tick(now, reset(2), Some(0)), None);
        assert_eq!(pace_tick(now, None, Some(14_400)), None);
    }

    fn forecast(at: DateTime<Utc>, left: f64) -> QuotaForecast {
        let point = |at, left| ForecastPoint { at, remaining_percent: left, used_percent: 100.0 - left };
        QuotaForecast {
            window_key: "weekly".into(),
            account_id: Some("a".into()),
            scope_model: Some("opus".into()),
            drain_pct_per_hour: 10.0,
            projected_exhaustion_at: Some(at + TimeDelta::hours(5)),
            resets_before_exhaustion: true,
            lookback_hours: 6,
            horizon_hours: 12,
            points: vec![point(at, left), point(at + TimeDelta::hours(1), left - 10.0), point(at + TimeDelta::hours(12), 95.0)],
            is_estimate: true,
        }
    }

    #[test]
    fn meter_estimates_the_pre_reset_balance_only_for_the_same_reading() {
        let now = DateTime::UNIX_EPOCH;
        let f = forecast(now, 50.0);
        assert_eq!(window_end_estimate(&f, 50.0, now + TimeDelta::hours(4), now), Some((10.0, false)));
        assert_eq!(window_end_estimate(&f, 50.0, now + TimeDelta::hours(5), now), Some((0.0, false)));
        assert_eq!(window_end_estimate(&f, 50.0, now + TimeDelta::hours(6), now), Some((0.0, true)));
        assert_eq!(window_end_estimate(&f, 60.0, now + TimeDelta::hours(4), now), None);
    }

    #[test]
    fn projection_is_drawn_only_when_the_future_is_enabled() {
        fn app() -> Element {
            let now = DateTime::UNIX_EPOCH;
            let history = TimeBounds { start: now - TimeDelta::hours(6), end: now };
            rsx! {
                for forecast_until in [None, Some(now + TimeDelta::hours(12))] {
                    QuotaHistoryChart { samples: vec![], forecasts: vec![forecast(now, 50.0)], names: BTreeMap::new(),
                        scope: QuotaChartScope { history, forecast_until, history_truncated: false, pool: true } }
                }
            }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert_eq!(rendered.matches("M229.33").count(), 1); // Now is one-third of a 6h history + 12h forecast.
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
            cache_hit_rate_observed: None,
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
    fn empty_activity_charts_show_no_range_data_without_hover_controls() {
        fn app() -> Element {
            rsx! {
                ActivityTrendChart { points: vec![], metric: Metric::Requests }
                CacheHitRateChart { points: vec![daily_point(0, 0)] }
            }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert_eq!(rendered.matches("No data for selected range").count(), 2);
        assert!(!rendered.contains("Hover"));
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
                    scope_model: None,
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
                    scope_model: None,
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
                    cache_hit_rate_observed: Some(0.75),
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
                    cache_hit_rate_observed: Some(0.80),
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
                QuotaHistoryChart { samples, forecasts: vec![], names: BTreeMap::from([("ag-a".into(), "Alpha".into())]),
                    scope: QuotaChartScope {
                        history: TimeBounds { start: from, end: now }, forecast_until: None, history_truncated: true, pool: false,
                    } }
                CacheHitRateChart { points: trend.clone() }
                ActivityTrendChart { points: trend, metric: Metric::CacheHitRate }
                ActivityHeatmap { cells, metric: Metric::CacheHitRate }
                DriftBandChart { days: drift_days, markers: vec![] }
            }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("chart-stage"));
        assert!(rendered.contains("Showing newest 4,000 samples"));
        assert!(rendered.contains("55.0% left"));
        assert!(!rendered.contains("Estimated projection data table"));
        assert!(rendered.contains("chart-readout"));
        assert!(rendered.contains("chart-hit"));
        assert!(rendered.contains("Daily cache hit rate (%)"));
        assert!(rendered.contains("Cache hit rate (%)"));
    }
}
