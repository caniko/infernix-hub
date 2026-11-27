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
            if value == 0.0 {
                "$0".into()
            } else if value >= 100.0 {
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
                rect { x: "{end}", y: "2", width: "{width - end}", height: "6", fill: "url(#{hatch})", class: "quota-projected-end" }
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
}

fn midnight(at: DateTime<Utc>) -> DateTime<Utc> {
    at.date_naive().and_time(chrono::NaiveTime::MIN).and_utc()
}

const TIME_LABEL_CHAR_PX: f64 = 6.7;

fn tick_label_anchor(x: f64, left: f64, right: f64, label: &str) -> &'static str {
    let half_label_px = label.len() as f64 * TIME_LABEL_CHAR_PX / 2.0;
    if x - left < half_label_px {
        "start"
    } else if right - x < half_label_px {
        "end"
    } else {
        "middle"
    }
}

fn time_ticks(view: TimeBounds, width: f64, compact: bool, dates: bool) -> Vec<(DateTime<Utc>, String)> {
    let hourly = !dates && view.end - view.start <= TimeDelta::hours(48);
    let hour_offset = |at: DateTime<Utc>| {
        TimeDelta::seconds(at.timestamp().rem_euclid(3600)) + TimeDelta::nanoseconds(i64::from(at.timestamp_subsec_nanos()))
    };
    let count = if compact || width < 200.0 {
        3
    } else if width < 360.0 && dates {
        4
    } else {
        5
    };
    let crosses_midnight = view.start.date_naive() != view.end.date_naive();
    let labels = |times: &[DateTime<Utc>]| {
        times
            .iter()
            .enumerate()
            .map(|(i, &at)| {
                let rollover = i == 0 || at.date_naive() != times[i - 1].date_naive();
                let format = if dates {
                    "%b %d"
                } else if crosses_midnight && rollover {
                    "%b %d %H:%M"
                } else {
                    "%H:%M"
                };
                (at, at.format(format).to_string())
            })
            .collect::<Vec<_>>()
    };
    let mut times = vec![view.start, view.end];
    let mut ticks = labels(&times);
    let boundaries: Vec<_> = (1..=2).map(|day| midnight(view.start) + TimeDelta::days(day)).filter(|&at| !dates && at < view.end).collect();
    let mut regular: Vec<_> = (1..count - 1)
        .map(|i| view.at(f64::from(i) / f64::from(count - 1)))
        .map(|at| {
            if dates {
                midnight(at + TimeDelta::hours(12))
            } else if hourly {
                at - hour_offset(at)
            } else {
                at
            }
        })
        .collect();
    // Keep rollover ticks first, and fill gaps rather than clustering regular ticks around midnight.
    regular.sort_by_key(|at| std::cmp::Reverse(times.iter().chain(&boundaries).map(|anchor| (*anchor - *at).abs()).min()));
    for at in boundaries.into_iter().chain(regular) {
        if times.len() == count as usize {
            break;
        }
        let index = times.partition_point(|&t| t < at);
        if index == 0 || index == times.len() || times[index] == at || (dates && times.iter().any(|t| t.date_naive() == at.date_naive())) {
            continue;
        }
        times.insert(index, at);
        let mut candidate = labels(&times);
        let last = candidate.len() - 1;
        let fits = |pair: &[(DateTime<Utc>, String)]| {
            let [(a, left_label), (b, right_label)] = pair else { unreachable!() };
            if left_label.is_empty() || right_label.is_empty() {
                return true;
            }
            let (a, b) = (view.fraction(*a) * width, view.fraction(*b) * width);
            let left_extent = match tick_label_anchor(a, 0.0, width, left_label) {
                "start" => 1.0,
                "end" => 0.0,
                _ => 0.5,
            } * left_label.len() as f64;
            let right_extent = match tick_label_anchor(b, 0.0, width, right_label) {
                "end" => 1.0,
                "start" => 0.0,
                _ => 0.5,
            } * right_label.len() as f64;
            b - a >= (left_extent + right_extent) * TIME_LABEL_CHAR_PX + 6.0
        };
        // Keep the endpoint gridlines at the viewport bounds, but prefer a whole-hour label when they collide.
        if hourly {
            if hour_offset(view.start) != TimeDelta::zero() && !fits(&candidate[..2]) {
                candidate[0].1.clear();
            }
            if hour_offset(view.end) != TimeDelta::zero() && !fits(&candidate[last - 1..]) {
                candidate[last].1.clear();
            }
        }
        if candidate.windows(2).all(fits) {
            ticks = candidate;
        } else {
            times.remove(index);
        }
    }
    ticks
}

#[component]
pub(super) fn TimeAxis(
    plot: TimePlotBox,
    view: TimeBounds,
    #[props(default)] date_only: bool,
    #[props(default)] compact: bool,
    #[props(default = 24.0)] label_offset: f64,
    #[props(default)] grid: bool,
) -> Element {
    let width = plot.right - plot.left;
    if date_only {
        let first = midnight(view.start);
        let first = if first < view.start { first + TimeDelta::days(1) } else { first };
        let count = (view.end - first).num_days() + 1;
        let limit = (width / 64.0).floor().max(1.0) as usize;
        let stride = (count.max(1) as usize).div_ceil(limit);
        return rsx! {
            g { class: "time-axis", pointer_events: "none",
                for day in (0..count).step_by(stride) {
                    {
                        let at = first + TimeDelta::days(day);
                        let x = plot.x(view, at);
                        let anchor = if x > plot.right - 26.0 { "end" } else { "middle" };
                        rsx! {
                            line { x1: "{x}", x2: "{x}", y1: "{plot.top}", y2: "{plot.bottom}", class: "chart-gridline subtle time-gridline" }
                            text { x: "{x}", y: "{plot.bottom + 17.0}", text_anchor: anchor, "{at.format(\"%b %d\")}" }
                        }
                    }
                }
            }
        };
    }
    let ticks = time_ticks(view, width, compact, view.end - view.start > TimeDelta::hours(48));
    let (y1, y2) = if grid { (plot.top, plot.bottom) } else { (plot.bottom, plot.bottom + 4.0) };
    rsx! {
        g { class: "time-axis", pointer_events: "none",
            for (at, label) in &ticks {
                {
                    let x = plot.x(view, *at);
                    let label_x = x.clamp(plot.left, plot.right);
                    let anchor = tick_label_anchor(label_x, plot.left, plot.right, label);
                    rsx! {
                        line { x1: "{x}", x2: "{x}", y1: "{y1}", y2: "{y2}",
                            class: if grid { "chart-gridline subtle time-gridline" } else { "chart-gridline" } }
                         if !label.is_empty() {
                             text { x: "{label_x}", y: "{plot.bottom + label_offset}", text_anchor: anchor, "{label}" }
                         }
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

fn quota_defaults_to_multiples(pool: bool, series: usize) -> bool {
    pool && series > 6
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
    let mut multiples = use_signal(|| None);
    let future = full.end > as_of;
    use_effect(use_reactive((&future,), move |_| scrub.reset()));
    let focus_key = focus.read();
    let selected = series.iter().position(|s| focus_key.as_ref() == Some(&s.key));
    let view = scrub.bounds(full);
    let has_layout_choice = pool && series.len() >= 4;
    let small = has_layout_choice && multiples().unwrap_or_else(|| quota_defaults_to_multiples(pool, series.len()));
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
        div { class: "row quota-plot-controls quota-legend-slot",
            if has_layout_choice {
                div { class: "segmented", role: "group", aria_label: "Quota chart layout",
                    button { class: "seg-btn", aria_pressed: !small, onclick: move |_| { multiples.set(Some(false)); scrub.clear(); }, "Overlay" }
                    button { class: "seg-btn", aria_pressed: small, onclick: move |_| { multiples.set(Some(true)); scrub.clear(); }, "Small multiples" }
                }
            }
            if view != full {
                button { class: "filter-chip", onclick: move |_| scrub.reset(), "Reset zoom" }
            }
            if future {
                span { class: "chart-legend-item", span { class: "chart-swatch dashed" } "Forecast (estimate)" }
            }
            if series.iter().any(|s| s.resets.iter().any(|&at| view.contains(at))) {
                span { class: "chart-legend-item", "◆ Reset" }
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
    let height = plot.bottom + 33.0;
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
                    TimeAxis { plot, view, compact: single.is_some(), grid: true }
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

pub(super) fn compact_window_pill_label(label: &str, window_key: &str) -> String {
    let short = |key: &str| match key.trim().to_ascii_lowercase().as_str() {
        "gemini-weekly" | "gemini weekly" => Some("Gem Wk"),
        "gemini-5h" | "gemini 5h" => Some("Gem 5h"),
        "3p-weekly" | "3p weekly" => Some("3P Wk"),
        "3p-5h" | "3p 5h" => Some("3P 5h"),
        _ => None,
    };
    short(window_key).or_else(|| short(label)).unwrap_or(if label.trim().is_empty() { window_key } else { label }).trim().into()
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct UnifiedPoolPoint {
    pub at: DateTime<Utc>,
    pub remaining_percent: f64,
    pub remaining_capacity_units: Option<f64>,
    pub total_capacity_units: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct UnifiedPoolSeries {
    pub window_key: String,
    pub scope_model: Option<String>,
    pub label: String,
    pub pill_label: String,
    pub is_weekly: bool,
    pub color_index: usize,
    pub points: Vec<UnifiedPoolPoint>,
    pub forecast: Option<QuotaForecast>,
    pub current_remaining_percent: Option<f64>,
    pub remaining_capacity_units: Option<f64>,
    pub total_capacity_units: Option<f64>,
    pub next_reset_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TierBandPoint {
    pub at: DateTime<Utc>,
    pub y0: f64,
    pub y1: f64,
    pub observed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TierBand {
    pub tier_label: String,
    pub tier_multiplier: f64,
    pub account_count: usize,
    pub total_capacity_units: f64,
    pub color_index: usize,
    pub points: Vec<TierBandPoint>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TierSplitData {
    pub window_key: String,
    pub label: String,
    pub total_capacity_units: f64,
    pub bands: Vec<TierBand>,
    pub total_points: Vec<(DateTime<Utc>, f64)>,
    pub forecast_units: Vec<(DateTime<Utc>, f64)>,
    pub next_reset_at: Option<DateTime<Utc>>,
}

fn tier_weight(account: &ModelAccount) -> f64 {
    account.tier_multiplier.filter(|m| m.is_finite() && *m > 0.0).unwrap_or(1.0)
}

fn tidy_capacity(units: f64) -> String {
    format!("{units:.2}").trim_end_matches('0').trim_end_matches('.').into()
}

fn eligible_accounts(report: &ProviderQuotaReport) -> impl Iterator<Item = &ModelAccount> {
    let enabled = report.accounts.iter().any(|a| a.enabled);
    report.accounts.iter().filter(move |a| !enabled || a.enabled)
}

fn tier_groups(report: &ProviderQuotaReport) -> BTreeMap<i64, Vec<&ModelAccount>> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for account in eligible_accounts(report) {
        groups.entry((tier_weight(account) * 100.0).round() as i64).or_default().push(account);
    }
    groups
}

fn matching_pool<'a>(report: &'a ProviderQuotaReport, key: &str, model: Option<&str>) -> Option<&'a PoolWindowSummary> {
    report.pools.iter().find(|p| p.window_key == key && p.scope_model.as_deref() == model)
}

fn pool_forecast<'a>(
    report: &'a ProviderQuotaReport,
    pool: Option<&'a PoolWindowSummary>,
    key: &str,
    model: Option<&str>,
) -> Option<&'a QuotaForecast> {
    pool.and_then(|p| p.forecast.as_ref())
        .filter(|f| f.account_id.is_none())
        .or_else(|| report.forecasts.iter().find(|f| f.account_id.is_none() && f.window_key == key && f.scope_model.as_deref() == model))
}

fn projection_points(forecast: Option<&QuotaForecast>, now: DateTime<Utc>, scale: f64) -> Vec<(DateTime<Utc>, f64)> {
    let mut points = forecast
        .into_iter()
        .flat_map(|f| &f.points)
        .filter(|p| p.at >= now && p.remaining_percent.is_finite())
        .map(|p| (p.at, (p.remaining_percent.clamp(0.0, 100.0) / 100.0) * scale))
        .collect::<Vec<_>>();
    points.sort_by_key(|p| p.0);
    points.dedup_by_key(|p| p.0);
    points
}

// Match the account chart's history gap limit; additionally expire readings after a reset.
const POOL_READING_MAX_AGE: TimeDelta = TimeDelta::hours(4);
const POOL_RESET_GRACE: TimeDelta = TimeDelta::minutes(15);

fn pool_reading_is_fresh(observed: DateTime<Utc>, reset: Option<DateTime<Utc>>, at: DateTime<Utc>) -> bool {
    at - observed <= POOL_READING_MAX_AGE && reset.is_none_or(|reset| at - reset <= POOL_RESET_GRACE)
}

fn pool_readings<'a>(report: &'a ProviderQuotaReport, key: &str, model: Option<&str>) -> BTreeMap<&'a str, Vec<Reading>> {
    let mut readings = BTreeMap::<_, Vec<_>>::new();
    for row in &report.samples {
        if row.window_key == key
            && row.scope_model.as_deref() == model
            && row.observed_at <= report.as_of
            && !row.stale
            && row.observation_state == QuotaObservationState::Measured
            && let (Some(left), _) = quota_values(row.observation_state, row.remaining_percent, row.used_percent)
        {
            readings.entry(row.account_id.as_str()).or_default().push(Reading {
                at: row.observed_at,
                left: left.clamp(0.0, 100.0),
                reset: row.resets_at,
            });
        }
    }
    for points in readings.values_mut() {
        points.sort_by_key(|p| p.at);
    }
    readings
}

fn pool_latest(points: &[Reading], at: DateTime<Utc>) -> Option<Reading> {
    let point = *points.get(points.partition_point(|p| p.at <= at).checked_sub(1)?)?;
    (point.at == at || pool_reading_is_fresh(point.at, point.reset, at)).then_some(point)
}

pub(super) fn build_unified_pool_series(report: &ProviderQuotaReport) -> Vec<UnifiedPoolSeries> {
    let mut windows = Vec::new();
    let mut seen = BTreeSet::new();
    for pool in &report.pools {
        if seen.insert((pool.window_key.as_str(), pool.scope_model.as_deref())) {
            windows.push((pool.window_key.as_str(), pool.scope_model.as_deref(), pool.label.as_str(), pool.kind, Some(pool)));
        }
    }
    if windows.is_empty() {
        for row in &report.samples {
            if seen.insert((row.window_key.as_str(), row.scope_model.as_deref())) {
                windows.push((row.window_key.as_str(), row.scope_model.as_deref(), row.label.as_str(), row.kind, None));
            }
        }
    }
    windows.sort_by_key(|(_, model, _, kind, _)| model.is_some() || !matches!(kind, QuotaWindowKind::Session | QuotaWindowKind::Weekly));
    let eligible: BTreeSet<_> = eligible_accounts(report).map(|account| account.id.as_str()).collect();
    let weights: BTreeMap<_, _> = report.accounts.iter().map(|account| (account.id.as_str(), tier_weight(account))).collect();
    windows
        .into_iter()
        .enumerate()
        .map(|(color_index, (key, model, label, kind, pool))| {
            let mut readings = pool_readings(report, key, model);
            if !eligible.is_empty() {
                readings.retain(|account, _| eligible.contains(account));
            }
            let aggregate = |at| {
                let mut total = 0.0;
                let mut remaining = 0.0;
                let mut resets_at = None;
                for (account, points) in &readings {
                    if let Some(point) = pool_latest(points, at) {
                        let weight = weights.get(account).copied().unwrap_or(1.0);
                        total += weight;
                        remaining += weight * (point.left / 100.0);
                        if let Some(reset) = point.reset.filter(|&reset| reset > at) {
                            resets_at = Some(resets_at.map_or(reset, |other: DateTime<Utc>| other.min(reset)));
                        }
                    }
                }
                (total > 0.0 && total.is_finite()).then_some(UnifiedPoolPoint {
                    at,
                    remaining_percent: remaining / total * 100.0,
                    remaining_capacity_units: Some(remaining),
                    total_capacity_units: Some(total),
                    resets_at,
                })
            };
            let mut points: Vec<_> =
                readings.values().flatten().map(|point| point.at).collect::<BTreeSet<_>>().into_iter().filter_map(aggregate).collect();
            let live_history = aggregate(report.as_of);
            let live_summary = pool.and_then(|p| {
                Some(UnifiedPoolPoint {
                    at: report.as_of,
                    remaining_percent: quota_values(QuotaObservationState::Measured, p.weighted_remaining_percent, p.weighted_used_percent)
                        .0?
                        .clamp(0.0, 100.0),
                    remaining_capacity_units: p.remaining_capacity_units.filter(|v| v.is_finite() && *v >= 0.0),
                    total_capacity_units: p.total_capacity_units.filter(|v| v.is_finite() && *v > 0.0),
                    resets_at: p.next_reset_at,
                })
            });
            if points.is_empty() {
                points.extend(live_summary.clone());
            }
            let current = live_summary.or(live_history);
            let scope_label = |label: &str| model.map_or_else(|| label.to_string(), |model| format!("{label} · {model}"));
            UnifiedPoolSeries {
                window_key: key.into(),
                scope_model: model.map(str::to_owned),
                label: scope_label(if label.trim().is_empty() { key } else { label.trim() }),
                pill_label: scope_label(&compact_window_pill_label(label, key)),
                is_weekly: kind == QuotaWindowKind::Weekly || key.contains("weekly"),
                color_index,
                points,
                forecast: pool_forecast(report, pool, key, model).cloned(),
                current_remaining_percent: current.as_ref().map(|p| p.remaining_percent),
                remaining_capacity_units: current.as_ref().and_then(|p| p.remaining_capacity_units),
                total_capacity_units: current.as_ref().and_then(|p| p.total_capacity_units),
                next_reset_at: pool.and_then(|p| p.next_reset_at).or_else(|| current.and_then(|p| p.resets_at)),
            }
        })
        .collect()
}

pub(super) fn build_tier_split_series(report: &ProviderQuotaReport, window_key: &str, scope_model: Option<&str>) -> Option<TierSplitData> {
    let groups = tier_groups(report);
    if groups.len() < 2 {
        return None;
    }
    let pool = matching_pool(report, window_key, scope_model);
    let mut readings = pool_readings(report, window_key, scope_model);
    let accounts: BTreeSet<_> = groups.values().flatten().map(|a| a.id.as_str()).collect();
    readings.retain(|account, _| accounts.contains(account));
    if readings.is_empty() {
        for account in groups.values().flatten() {
            if let Some(window) = account.windows.iter().find(|w| w.window_key == window_key && w.scope_model.as_deref() == scope_model)
                && !window.stale
                && window.observation_state == QuotaObservationState::Measured
                && window.observed_at.is_none_or(|at| at <= report.as_of && pool_reading_is_fresh(at, window.resets_at, report.as_of))
                && window.resets_at.is_none_or(|at| report.as_of - at <= POOL_RESET_GRACE)
                && let Some(left) = quota_values(window.observation_state, window.remaining_percent, window.used_percent).0
            {
                readings
                    .insert(account.id.as_str(), vec![Reading { at: report.as_of, left: left.clamp(0.0, 100.0), reset: window.resets_at }]);
            }
        }
    }
    let times: BTreeSet<_> = readings.values().flatten().map(|point| point.at).collect();
    let mut total_points: Vec<_> = times.into_iter().map(|at| (at, 0.0)).collect();
    let mut observed_capacity_at_now = 0.0;
    let mut bands = Vec::new();
    for (color_index, (key, accounts)) in groups.into_iter().rev().enumerate() {
        let multiplier = key as f64 / 100.0;
        let plan = accounts[0].plan.as_deref().map(str::trim).filter(|plan| {
            !plan.is_empty() && accounts.iter().all(|account| account.plan.as_deref().is_some_and(|p| p.trim().eq_ignore_ascii_case(plan)))
        });
        observed_capacity_at_now += accounts
            .iter()
            .filter(|a| readings.get(a.id.as_str()).and_then(|pts| pool_latest(pts, report.as_of)).is_some())
            .map(|a| tier_weight(a))
            .sum::<f64>();
        let points = total_points
            .iter_mut()
            .map(|(at, total)| {
                let y0 = *total;
                let mut observed = false;
                for a in &accounts {
                    if let Some(point) = readings.get(a.id.as_str()).and_then(|pts| pool_latest(pts, *at)) {
                        observed = true;
                        *total += tier_weight(a) * (point.left / 100.0);
                    }
                }
                TierBandPoint { at: *at, y0, y1: *total, observed }
            })
            .collect();
        bands.push(TierBand {
            tier_label: format!("{}× {}", tidy_capacity(multiplier), plan.unwrap_or("tier")),
            tier_multiplier: multiplier,
            account_count: accounts.len(),
            total_capacity_units: accounts.iter().map(|a| tier_weight(a)).sum(),
            color_index,
            points,
        });
    }
    let total_capacity_units: f64 = bands.iter().map(|band| band.total_capacity_units).sum();
    if !total_capacity_units.is_finite() || total_capacity_units <= 0.0 {
        return None;
    }
    let forecast_scale = if observed_capacity_at_now.is_finite() && observed_capacity_at_now > 0.0 {
        observed_capacity_at_now
    } else {
        pool.and_then(|p| p.total_capacity_units).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(total_capacity_units)
    };
    let next_reset_at = pool
        .and_then(|p| p.next_reset_at)
        .or_else(|| readings.values().filter_map(|points| pool_latest(points, report.as_of)?.reset.filter(|&at| at > report.as_of)).min());
    let label = pool
        .map(|p| p.label.as_str())
        .or_else(|| {
            report.samples.iter().find(|p| p.window_key == window_key && p.scope_model.as_deref() == scope_model).map(|p| p.label.as_str())
        })
        .unwrap_or(window_key);
    Some(TierSplitData {
        window_key: window_key.into(),
        label: label.into(),
        total_capacity_units,
        bands,
        total_points,
        forecast_units: projection_points(pool_forecast(report, pool, window_key, scope_model), report.as_of, forecast_scale),
        next_reset_at,
    })
}

fn interpolated_projection_at(points: &[(DateTime<Utc>, f64)], at: DateTime<Utc>) -> Option<f64> {
    let next = points.partition_point(|p| p.0 < at);
    let (to_at, to_val) = *points.get(next)?;
    if to_at == at {
        return Some(to_val);
    }
    let (from_at, from_val) = *points.get(next.checked_sub(1)?)?;
    let span = (to_at - from_at).as_seconds_f64();
    if span <= 0.0 {
        return Some(to_val);
    }
    let time = (at - from_at).as_seconds_f64() / span;
    Some(from_val + (to_val - from_val) * time)
}

fn svg_points(points: impl IntoIterator<Item = (f64, f64)>) -> String {
    let mut out = String::new();
    for (x, y) in points {
        let _ = write!(out, "{x:.2},{y:.2} ");
    }
    out
}

fn observed_pool_point(points: &[UnifiedPoolPoint], at: DateTime<Utc>) -> Option<&UnifiedPoolPoint> {
    let point = points.get(points.partition_point(|p| p.at <= at).checked_sub(1)?)?;
    (point.at == at || pool_reading_is_fresh(point.at, point.resets_at, at)).then_some(point)
}

fn remaining_text(pct: Option<f64>, remaining: Option<f64>, total: Option<f64>) -> String {
    let mut text = pct.map_or_else(|| "—".to_string(), |pct| format!("{pct:.1}%"));
    if let Some((remaining, total)) = remaining.zip(total) {
        let _ = write!(text, " · {remaining:.1} / {total:.1}×");
    }
    text
}

#[component]
pub(super) fn UnifiedPoolQuotaChart(report: ProviderQuotaReport, mut range_hours: Signal<u32>, mut show_forecast: Signal<bool>) -> Element {
    let series = use_memo(use_reactive((&report,), |(report,)| build_unified_pool_series(&report)));
    let capacity = use_memo(use_reactive((&report,), |(report,)| {
        let groups = tier_groups(&report);
        let total: f64 = groups.values().flatten().map(|a| tier_weight(a)).sum();
        (groups.len() > 1 && total.is_finite()).then_some(total)
    }));
    let mut selected = use_signal(|| None::<QuotaSeriesKey>);
    let mut split_mode = use_signal(|| false);
    let scrub = use_time_scrub();
    let measure = use_time_plot_measure();
    let clip_id = use_hook(|| format!("unified-pool-{}", Uuid::new_v4().simple()));
    let series = series.read();
    let primary = series.first().map(|s| (s.window_key.clone(), s.scope_model.clone()));
    let selected_key = selected();
    let focused = selected_key.as_ref().and_then(|(key, model)| series.iter().find(|s| s.window_key == *key && s.scope_model == *model));
    let has_focused = focused.is_some();
    let tier_key = focused.filter(|_| split_mode()).map(|s| (s.window_key.clone(), s.scope_model.clone()));
    let split = use_memo(use_reactive((&report, &tier_key), |(report, key)| {
        let (window, model) = key?;
        build_tier_split_series(&report, &window, model.as_deref())
    }));
    let split = split.read();
    let visible = || series.iter().filter(|s| focused.is_none_or(|window| window.color_index == s.color_index));
    let now = report.as_of;
    let projection = show_forecast();
    let forecasts: BTreeMap<_, _> = if projection && split.is_none() {
        visible().map(|s| (s.color_index, projection_points(s.forecast.as_ref(), now, 100.0))).collect()
    } else {
        BTreeMap::new()
    };
    let future_end = forecasts
        .values()
        .chain(split.iter().map(|tier| &tier.forecast_units).filter(|_| projection))
        .filter_map(|points| points.last().map(|p| p.0))
        .filter(|at| *at > now)
        .max();
    let range = range_hours();
    let full = TimeBounds { start: now - TimeDelta::hours(i64::from(range.max(1))), end: future_end.unwrap_or(now) };
    let view = scrub.bounds(full);
    let reset = split.as_ref().and_then(|tier| tier.next_reset_at).or_else(|| focused.and_then(|s| s.next_reset_at));
    let reset_mark = reset.filter(|at| future_end.is_some_and(|end| *at >= now && *at <= end));
    let mut visible_times = if let Some(tier) = split.as_ref() {
        tier.total_points.iter().map(|p| p.0).chain(tier.forecast_units.iter().filter(|_| projection).map(|p| p.0)).collect::<BTreeSet<_>>()
    } else {
        visible()
            .flat_map(|s| s.points.iter().map(|p| p.at).chain(s.current_remaining_percent.map(|_| now)))
            .chain(forecasts.values().flatten().map(|p| p.0))
            .collect()
    };
    visible_times.extend(reset_mark);
    let sorted: Rc<Vec<_>> = Rc::new(visible_times.into_iter().collect());
    let keyboard_times = sorted.clone();
    let active = scrub.active().filter(|at| view.contains(*at));
    let latest_tier = split.as_ref().and_then(|tier| tier.total_points.last()).filter(|p| full.contains(p.0));
    let at = active.or_else(|| latest_tier.map(|p| p.0)).unwrap_or(now);
    let timestamp = at.format("%b %d %H:%M UTC");
    let width = (measure.width)().max(100.0);
    let plot = TimePlotBox { left: 42.0, right: width - 14.0, top: 20.0, bottom: 196.0 };
    let axis_max = split.as_ref().map_or(100.0, |t| t.total_capacity_units);
    let x = |at| plot.x(view, at);
    let y = |value| chart_y(plot, value / axis_max);
    let coordinates = |&(at, value): &(DateTime<Utc>, f64)| (x(at), y(value));
    let reading = |s: &UnifiedPoolSeries| {
        if at > now {
            let pct = forecasts.get(&s.color_index).and_then(|points| interpolated_projection_at(points, at));
            let remaining = pct.zip(s.total_capacity_units).map(|(p, total)| p / 100.0 * total);
            (pct, remaining, s.total_capacity_units)
        } else if at == now {
            (s.current_remaining_percent, s.remaining_capacity_units, s.total_capacity_units)
        } else {
            observed_pool_point(&s.points, at)
                .map_or((None, None, None), |p| (Some(p.remaining_percent), p.remaining_capacity_units, p.total_capacity_units))
        }
    };
    let now_x = x(now).max(plot.left);
    let forecast_visible = future_end.is_some() && view.end > now;
    let time_label = if at > now {
        "Projected"
    } else if active.is_some() {
        "Observed"
    } else {
        "Latest"
    };
    rsx! {
        figure { class: "provider-chart chart-stage unified-plot-card",
            figcaption { class: "chart-header", "Remaining pool quota · UTC" }
            div { class: "row unified-plot-controls", style: "flex-wrap: wrap; gap: 6px",
                div { class: "segmented", role: "group", aria_label: "Quota window",
                    button { class: "seg-btn", aria_pressed: focused.is_none(),
                        onclick: move |_| { selected.set(None); split_mode.set(false); scrub.reset(); }, "All" }
                    for s in series.iter() {
                        {
                            let key = (s.window_key.clone(), s.scope_model.clone());
                            let pressed = focused.is_some_and(|window| window.color_index == s.color_index);
                            rsx! {
                                button { key: "{s.color_index}", class: "seg-btn", aria_pressed: pressed, title: "{s.label}",
                                    onclick: move |_| { selected.set(Some(key.clone())); scrub.reset(); }, "{s.pill_label}" }
                            }
                        }
                    }
                }
                if let Some(capacity) = capacity().filter(|_| primary.is_some()) {
                    div { class: "segmented", role: "group", aria_label: "Quota aggregation",
                        button { class: "seg-btn", aria_pressed: split.is_none(),
                            onclick: move |_| { split_mode.set(false); scrub.reset(); }, "Pool total" }
                        button { class: "seg-btn", aria_pressed: split.is_some(), title: "Tier split ({tidy_capacity(capacity)}×)",
                            onclick: move |_| {
                                if !has_focused { selected.set(primary.clone()); }
                                split_mode.set(true);
                                scrub.reset();
                            }, "Tier split ({tidy_capacity(capacity)}×)" }
                    }
                }
                div { class: "segmented", role: "group", aria_label: "Quota history range",
                    for (hours, label) in [(6, "6h"), (12, "12h"), (24, "24h"), (48, "48h"), (168, "7d")] {
                        button { key: "{hours}", class: "seg-btn", aria_pressed: range == hours,
                            onclick: move |_| { range_hours.set(hours); scrub.reset(); }, "{label}" }
                    }
                }
                button { class: "filter-chip", aria_pressed: projection,
                    onclick: move |_| { show_forecast.set(!projection); scrub.reset(); }, "Projection" }
                if projection && future_end.is_none() { small { class: "form-help", "No forecast" } }
            }
            div { class: "row unified-readout-bar", style: "flex-wrap: wrap; gap: 6px 12px",
                div { class: "row", style: "flex-wrap: wrap; gap: 6px 12px", role: "status",
                    strong { class: "mono", "{time_label} · {timestamp}" }
                    if let Some(tier) = split.as_ref() {
                        if at > now {
                            if let Some(units) = interpolated_projection_at(&tier.forecast_units, at) {
                                span { class: "mono", "Estimated total: {units:.1} / {axis_max:.1}×" }
                            }
                        } else if let Some(i) = tier.total_points.partition_point(|p| p.0 <= at).checked_sub(1)
                            && at - tier.total_points[i].0 <= POOL_READING_MAX_AGE
                        {
                            for band in &tier.bands {
                                {
                                    let point = band.points[i];
                                    let value = if point.observed {
                                        format!("{:.1} / {:.1}×", point.y1 - point.y0, band.total_capacity_units)
                                    } else {
                                        "—".to_string()
                                    };
                                    rsx! { span { key: "{band.color_index}", class: "mono", "{band.tier_label}: {value}" } }
                                }
                            }
                        } else {
                            span { class: "form-help", "No observed tier capacity" }
                        }
                    } else {
                        for s in visible() {
                            {
                                let (value, remaining, total) = reading(s);
                                rsx! { span { key: "{s.color_index}", class: "mono", "{s.pill_label}: {remaining_text(value, remaining, total)}" } }
                            }
                        }
                    }
                    if active.is_some() && active == reset_mark { span { "Scheduled reset" } }
                }
                if view != full { button { class: "filter-chip", onclick: move |_| scrub.reset(), "Reset zoom" } }
            }
            if full.timestamps(&sorted).is_empty() { p { class: "form-help", "No measured pool quota for this range." } }
            div { class: "quota-plot-surface", tabindex: "0", role: "group",
                aria_label: "Pool quota timeline. Left and right arrows inspect; plus or minus zooms; zero or Escape resets.",
                onkeydown: move |e| scrub.key(e, full, &keyboard_times),
                onblur: move |_| scrub.clear(),
                onmounted: move |e| measure.mount(e),
                onresize: move |_| measure.refresh(),
                onmouseenter: move |_| measure.refresh(),
                svg { class: "unified-plot-svg", role: "img", "aria-label": "Remaining pool quota over time",
                    width: "100%", height: "228", view_box: "0 0 {width} 228", preserve_aspect_ratio: "none",
                    defs { clipPath { id: "{clip_id}", rect { x: "{plot.left - 4.0}", y: "{plot.top - 4.0}",
                        width: "{plot.right - plot.left + 8.0}", height: "{plot.bottom - plot.top + 12.0}" } } }
                    if forecast_visible {
                        rect { x: "{now_x}", y: "{plot.top}", width: "{plot.right - now_x}", height: "{plot.bottom - plot.top}", class: "quota-future" }
                    }
                    ActivityGrid { plot, ticks: (0..=4).map(|i| {
                        let value = f64::from(i) * axis_max / 4.0;
                        if split.is_some() { format!("{}×", tidy_capacity(value)) } else { format!("{value:.0}%") }
                    }).collect() }
                    TimeAxis { plot, view, label_offset: 20.0 }
                    if forecast_visible && view.contains(now) {
                        line { x1: "{now_x}", x2: "{now_x}", y1: "{plot.top}", y2: "{plot.bottom}", class: "quota-now-line" }
                        text { x: "{now_x.clamp(plot.left + 14.0, plot.right - 14.0)}", y: "13", text_anchor: "middle",
                            style: "fill: var(--honey); font-weight: 700", "NOW" }
                    }
                    g { clip_path: "url(#{clip_id})", pointer_events: "none",
                        if let Some(tier) = split.as_ref() {
                            for (run_index, run) in tier.total_points.chunk_by(|a, b| b.0 - a.0 <= POOL_READING_MAX_AGE).enumerate() {
                                {
                                    let start = tier.total_points.partition_point(|p| p.0 < run[0].0);
                                    rsx! {
                                        g { key: "{run_index}",
                                            for band in &tier.bands {
                                                {
                                                    let points = &band.points[start..start + run.len()];
                                                    let upper = svg_points(points.iter().map(|p| (x(p.at), y(p.y1))));
                                                    let lower = svg_points(points.iter().rev().map(|p| (x(p.at), y(p.y0))));
                                                    let color = QuotaSeries::color(band.color_index, band.color_index == 0);
                                                    rsx! { path { key: "{band.color_index}", d: "M{upper}{lower}Z", fill: color, fill_opacity: "0.48" } }
                                                }
                                            }
                                            polyline { points: svg_points(run.iter().map(coordinates)), fill: "none", stroke: "var(--text)", stroke_width: "1.8" }
                                            if run.len() == 1 {
                                                circle { cx: "{x(run[0].0)}", cy: "{y(run[0].1)}", r: "3", fill: "var(--text)" }
                                            }
                                        }
                                    }
                                }
                            }
                            if forecast_visible && let [(at, amount)] = tier.forecast_units.as_slice() {
                                circle { cx: "{x(*at)}", cy: "{y(*amount)}", r: "3", fill: "none", stroke: "var(--text)", stroke_dasharray: "3 2" }
                            } else if forecast_visible {
                                polyline { points: svg_points(tier.forecast_units.iter().map(coordinates)), fill: "none",
                                    stroke: "var(--text)", stroke_width: "1.8", stroke_dasharray: "4 3" }
                            }
                        } else {
                            for s in visible() {
                                {
                                    let single = focused.is_some();
                                    let color = QuotaSeries::color(s.color_index, s.color_index == 0);
                                    let weight = if single { 2.5 } else if s.is_weekly { 2.4 } else { 1.7 };
                                    let opacity = if single || s.is_weekly { 1.0 } else { 0.8 };
                                    rsx! {
                                        g { key: "{s.color_index}",
                                            for (i, run) in s.points.chunk_by(|a, b| pool_reading_is_fresh(a.at, a.resets_at, b.at)).enumerate() {
                                                {
                                                    let points = svg_points(run.iter().map(|p| (x(p.at), y(p.remaining_percent))));
                                                    let fill = format!("{points} {},{} {},{}", x(run[run.len() - 1].at), plot.bottom, x(run[0].at), plot.bottom);
                                                    rsx! {
                                                        g { key: "{i}",
                                                            if single && run.len() > 1 { polygon { points: fill, fill: color, opacity: "0.14" } }
                                                            polyline { points, fill: "none", stroke: color, stroke_width: "{weight}", opacity: "{opacity}" }
                                                            if run.len() == 1 && (run[0].at != now || s.current_remaining_percent.is_none()) {
                                                                circle { cx: "{x(run[0].at)}", cy: "{y(run[0].remaining_percent)}", r: "2.6", fill: color }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            if forecast_visible && let Some(points) = forecasts.get(&s.color_index) {
                                                if let [(at, amount)] = points.as_slice() {
                                                    circle { cx: "{x(*at)}", cy: "{y(*amount)}", r: "3", fill: "none", stroke: color, stroke_dasharray: "3 2" }
                                                } else {
                                                    polyline { points: svg_points(points.iter().map(coordinates)), fill: "none",
                                                        stroke: color, stroke_width: "{weight}", stroke_dasharray: "4 3", opacity: "{opacity}" }
                                                }
                                            }
                                            if view.contains(now) && let Some(pct) = s.current_remaining_percent {
                                                circle { cx: "{x(now)}", cy: "{y(pct)}", r: if single { "4" } else { "3.2" }, fill: color }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(reset) = reset_mark.filter(|at| view.contains(*at)) {
                            path { d: "M{x(reset)} {plot.bottom - 4.0}l4 4-4 4-4-4z", fill: "var(--warn)" }
                        }
                    }
                    if let Some(at) = active {
                        TimeCrosshair { plot, x: x(at) }
                        if let Some(tier) = split.as_ref() {
                            {
                                let value = if at > now {
                                    interpolated_projection_at(&tier.forecast_units, at)
                                } else {
                                    tier.total_points.binary_search_by_key(&at, |p| p.0).ok().map(|i| tier.total_points[i].1)
                                };
                                rsx! {
                                    if let Some(value) = value {
                                        circle { cx: "{x(at)}", cy: "{y(value)}", r: "4", fill: "var(--text)", pointer_events: "none" }
                                    }
                                }
                            }
                        } else {
                            for s in visible() {
                                if let Some(value) = reading(s).0 {
                                    circle { key: "{s.color_index}", cx: "{x(at)}", cy: "{y(value)}",
                                        r: "4", fill: QuotaSeries::color(s.color_index, s.color_index == 0),
                                        stroke: "var(--panel)", stroke_width: "1.5", pointer_events: "none" }
                                }
                            }
                        }
                    }
                    TimePlotHit { plot, origin: measure.origin, full, sorted, scrub }
                }
            }
            div { class: "chart-legend unified-legend-bar",
                if let Some(tier) = split.as_ref() {
                    for band in &tier.bands {
                        span { key: "{band.color_index}", class: "chart-legend-item", style: "--chart-accent: {QuotaSeries::color(band.color_index, band.color_index == 0)}",
                            span { class: "chart-swatch", aria_hidden: "true" }
                            "{band.tier_label} · {band.account_count} accounts"
                            if latest_tier.is_some() && let Some(last) = band.points.last() {
                                if last.observed {
                                    " · {last.y1 - last.y0:.1} / {band.total_capacity_units:.1}×"
                                } else {
                                    " · —"
                                }
                            }
                        }
                    }
                    span { "Observed tier capacity only" }
                } else {
                    for s in visible() {
                        span { key: "{s.color_index}", class: "chart-legend-item", style: "--chart-accent: {QuotaSeries::color(s.color_index, s.color_index == 0)}",
                            span { class: "chart-swatch", aria_hidden: "true" } "{s.label} · {remaining_text(s.current_remaining_percent, None, None)}"
                        }
                    }
                }
                if future_end.is_some() { span { class: "chart-legend-item", span { class: "chart-swatch dashed" } "Projection (estimate)" } }
                if let Some(reset) = reset { span { "◆ Next reset: {date(Some(reset))}" } }
                span { class: "form-help", "UTC · ←/→ inspect · wheel or +/− zoom · drag to pan · 0 to reset" }
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
    window: String,
    value: Option<f64>,
    requests: u64,
    failures: u64,
    tokens: u64,
    cache_read: u64,
    cache_hit_rate: Option<f64>,
    cost_usd: f64,
}

pub(super) const NO_RANGE_DATA: &str = "No data for selected range";
const INSPECT: &str = "Hover or ←/→ to inspect";

pub(super) fn bucket_badge(seconds: Option<u64>) -> Option<&'static str> {
    match seconds {
        Some(3600) => Some("Hourly · 24h"),
        Some(10800) => Some("3-hour · 7d"),
        Some(86400) => Some("Daily"),
        _ => None,
    }
}

fn bucket_bounds(points: &[ActivityTrendPoint], seconds: u64) -> Option<TimeBounds> {
    Some(TimeBounds { start: points.first()?.bucket_start, end: points.last()?.bucket_start + TimeDelta::seconds(seconds as i64) })
}

fn bucket_window(at: DateTime<Utc>, seconds: u64) -> String {
    let format = if seconds == 86400 { "%Y-%m-%d" } else { "%b %d %H:%M" };
    let start = at.format(format);
    let end = at + TimeDelta::seconds(seconds as i64);
    let end = end.format(if seconds != 86400 && end.date_naive() == at.date_naive() { "%H:%M" } else { format });
    format!("{start} → {end} UTC")
}

fn trend_buckets(mut points: Vec<ActivityTrendPoint>, bucket_seconds: Option<u64>) -> (u64, Vec<ActivityTrendPoint>) {
    let seconds = bucket_seconds.filter(|s| matches!(s, 3600 | 10800 | 86400)).unwrap_or(86400);
    points.sort_by_key(|p| p.bucket_start);
    if bucket_seconds.is_some() {
        // Adaptive reports are already dense, including buckets with no requests.
        return (seconds, points);
    }
    let mut dense = Vec::<ActivityTrendPoint>::with_capacity(points.len());
    for mut p in points {
        if let Some(previous) = dense.last() {
            let days = (p.bucket_start - previous.bucket_start).num_days();
            if (2..=400).contains(&days) {
                let start = previous.bucket_start;
                dense.extend((1..days).map(|offset| ActivityTrendPoint {
                    bucket_start: start + TimeDelta::days(offset),
                    label: String::new(),
                    requests: 0,
                    failures: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cache_hit_rate: 0.0,
                    cache_hit_rate_observed: None,
                    thinking_tokens: 0,
                    api_equivalent_cost_usd: 0.0,
                }));
            }
        }
        p.cache_hit_rate_observed =
            p.cache_hit_rate_observed.or_else(|| effective_cache_hit_rate(p.input_tokens, p.cache_read_tokens, p.cache_hit_rate));
        dense.push(p);
    }
    (seconds, dense)
}

fn activity_plot(width: f64) -> TimePlotBox {
    TimePlotBox { left: 48.0, right: width - 12.0, top: 16.0, bottom: 146.0 }
}

fn chart_y(plot: TimePlotBox, fraction: f64) -> f64 {
    plot.bottom - (plot.bottom - plot.top) * fraction.clamp(0.0, 1.0)
}

#[component]
fn ActivityGrid(plot: TimePlotBox, ticks: Vec<String>) -> Element {
    rsx! {
        for (i, tick) in ticks.iter().enumerate() {
            {
                let y = chart_y(plot, i as f64 / (ticks.len() - 1) as f64);
                rsx! {
                    line { x1: "{plot.left}", x2: "{plot.right}", y1: "{y}", y2: "{y}",
                        class: if i == 0 { "chart-gridline" } else { "chart-gridline subtle" } }
                    text { x: "{plot.left - 6.0}", y: "{y + 3.5}", text_anchor: "end", "{tick}" }
                }
            }
        }
    }
}

#[component]
fn RhythmTicks(plot: TimePlotBox, labels: Vec<String>) -> Element {
    let width = plot.right - plot.left;
    let step = width / labels.len().max(1) as f64;
    let stride = if step >= 10.0 {
        4
    } else if step >= 7.0 {
        6
    } else {
        (52.0 / step).ceil() as usize
    };
    rsx! {
        for (i, label) in labels.iter().enumerate() {
            {
                let x = plot.left + (i as f64 + 0.5) * step;
                if x + 23.0 > plot.right + 12.0 || i % stride != 0 {
                    rsx! {}
                } else {
                    rsx! {
                        line { x1: "{x}", x2: "{x}", y1: "{plot.bottom}", y2: "{plot.bottom + 4.0}", class: "chart-gridline" }
                        text { x: "{x}", y: "{plot.bottom + 17.0}", text_anchor: "middle", "{label}" }
                    }
                }
            }
        }
    }
}

fn step_bucket(e: KeyboardEvent, mut active: Signal<Option<usize>>, count: usize) {
    if count == 0 {
        return;
    }
    let i = match e.key() {
        Key::Named(NamedKey::ArrowLeft) => active.peek().map_or(count - 1, |i| i.saturating_sub(1)),
        Key::Named(NamedKey::ArrowRight) => active.peek().map_or(0, |i| (i + 1).min(count - 1)),
        _ => return,
    };
    e.prevent_default();
    active.set(Some(i));
}

#[component]
fn BucketHit(plot: TimePlotBox, index: usize, count: usize, mut active: Signal<Option<usize>>) -> Element {
    let step = (plot.right - plot.left) / count as f64;
    rsx! { rect { class: "chart-hit", x: "{plot.left + index as f64 * step}", y: "{plot.top}",
        width: "{step}", height: "{plot.bottom - plot.top + 20.0}", fill: "transparent",
        onmouseenter: move |_| active.set(Some(index)),
    } }
}

#[component]
fn ActivityReadout(right: bool, heading: String, fields: Vec<(&'static str, String)>) -> Element {
    rsx! {
        div { class: if right { "activity-readout right" } else { "activity-readout" },
            strong { "{heading}" }
            dl { for (label, value) in fields { dt { "{label}" } dd { "{value}" } } }
        }
    }
}

#[component]
pub(super) fn ShareBar(
    label: String,
    value: f64,
    total: f64,
    #[props(default = "of listed total")] scale_label: &'static str,
    #[props(default)] show_share: bool,
) -> Element {
    let share = (total.is_finite() && total > 0.0 && value.is_finite()).then(|| (value / total).clamp(0.0, 1.0) * 100.0);
    rsx! {
        span { class: if show_share { "share-bar-cell share-bar-labeled" } else { "share-bar-cell" },
            span { "{label}" }
            if let Some(pct) = share {
                span { class: "share-bar-track", aria_hidden: "true", title: "{pct:.1}% {scale_label}",
                    span { class: if pct > 0.0 { "nonzero" } else { "" }, style: "width: {pct:.2}%" }
                }
                if show_share {
                    span { class: "share-bar-percent", aria_label: "{pct:.1}% {scale_label}", "{pct:.1}%" }
                } else {
                    span { class: "sr-only", ", {pct:.1}% {scale_label}" }
                }
            }
        }
    }
}

#[component]
fn InteractiveBars(
    label: &'static str,
    metric: Metric,
    items: Vec<BarItem>,
    #[props(default)] rhythm: bool,
    #[props(default)] time: Option<TimeBounds>,
) -> Element {
    let mut hovered = use_signal(|| None::<usize>);
    let measure = use_time_plot_measure();
    let width = (measure.width)().max(100.0);
    let plot = TimePlotBox { bottom: if rhythm { 118.0 } else { 146.0 }, ..activity_plot(width) };
    let (plot_left, plot_bottom) = (plot.left, plot.bottom);
    let count = items.len();
    let step = (plot.right - plot_left) / count.max(1) as f64;
    let unit = metric.label();
    let active_idx = hovered().filter(|&i| i < count);
    let peak = items.iter().enumerate().filter_map(|(i, p)| Some((i, p.value?))).max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));
    let peak_idx = peak.filter(|&(_, v)| v > 0.0 || metric == Metric::CacheHitRate).map(|(i, _)| i);
    let max = peak.map_or(0.0, |(_, v)| v);
    let scale = match metric {
        Metric::CacheHitRate => 100.0,
        Metric::Cost => max.max(0.002),
        _ => (max / 2.0).ceil().max(1.0) * 2.0,
    };
    let height = plot_bottom + 24.0;
    rsx! {
        if items.is_empty() {
            p { class: "form-help", "{NO_RANGE_DATA}" }
        } else {
            figure { class: "provider-chart chart-stage",
                figcaption { class: "chart-header",
                    span { "{label} · {unit}" }
                    if let Some(i) = peak_idx {
                        {
                            let item = &items[i];
                            let time = item.window.split_once(" → ").map_or(item.window.as_str(), |(start, _)| start);
                            let suffix = match metric { Metric::Requests => " req", Metric::Tokens => " tok", _ => "" };
                            rsx! { span { class: "activity-peak mono", "Peak: {time} · {metric.format_value(item.value)}{suffix}" } }
                        }
                    }
                    if rhythm && items.iter().any(|item| item.failures > 0) {
                        span { class: "chart-legend-item form-help", span { class: "chart-swatch outlier", aria_hidden: "true" } "Hour with ≥1 failure" }
                    }
                    span { class: "chart-inspect form-help", "{INSPECT}" }
                }
                div { class: "activity-plot", tabindex: "0", role: "group",
                    aria_label: "{label}, {unit}. Use left and right arrows to inspect. Data table follows.",
                    onkeydown: move |e| step_bucket(e, hovered, count),
                    onblur: move |_| hovered.set(None),
                    onmouseleave: move |_| hovered.set(None),
                    onmounted: move |e| measure.mount(e),
                    onresize: move |_| measure.refresh(),
                    svg { width: "100%", height: "{height}", view_box: "0 0 {width} {height}", preserve_aspect_ratio: "none", "aria-hidden": "true",
                        ActivityGrid { plot, ticks: [0.0, 0.5, 1.0].map(|f| fmt_compact_axis(metric, scale * f)).to_vec() }
                        if let Some(view) = time {
                            TimeAxis { plot, view, label_offset: 17.0 }
                        } else if rhythm {
                            RhythmTicks { plot, labels: items.iter().map(|p| p.window.clone()).collect() }
                        }
                        for (i, item) in items.iter().enumerate() {
                            {
                                let x = plot_left + (i as f64 + 0.5) * step;
                                let bar_w = (step * 0.74).min(34.0);
                                let y = chart_y(plot, item.value.unwrap_or_default() / scale);
                                let peak_class = if peak_idx == Some(i) { "peak" } else { "" };
                                rsx! {
                                    if item.value.is_some() {
                                        rect { x: "{x - bar_w / 2.0}", y: "{y}", width: "{bar_w}", height: "{plot_bottom - y}", rx: "2",
                                            class: "activity-bar {peak_class}" }
                                    }
                                    if rhythm && item.failures > 0 {
                                        circle { cx: "{x}", cy: "{(y - 5.0).max(4.0)}", r: "2.4", class: "chart-outlier" }
                                    }
                                    if active_idx == Some(i) { TimeCrosshair { plot, x } }
                                    BucketHit { plot, index: i, count, active: hovered }
                                }
                            }
                        }
                    }
                    div { role: "status",
                        if let Some(i) = active_idx {
                            {
                                let item = &items[i];
                                rsx! { ActivityReadout { right: i < count / 2, heading: item.window.clone(), fields: vec![
                                    ("Requests", fmt_count(item.requests)), ("Tokens", fmt_count(item.tokens)),
                                    ("API-equivalent USD", format!("${:.4}", item.cost_usd)), ("Failures", fmt_count(item.failures)),
                                    ("Cache hit", fmt_hit_pct(item.cache_hit_rate)), ("Cache read", fmt_count(item.cache_read)),
                                ] } }
                            }
                        }
                    }
                }
                details { summary { "Data table" }
                    Table { label, headers: &["Period", "Value", "Requests", "Tokens", "Failures", "Cache hit %", "API-equivalent USD"],
                        rows: items.iter().map(|p| vec![
                            p.window.clone(),
                            metric.format_value(p.value),
                            fmt_count(p.requests),
                            fmt_count(p.tokens),
                            fmt_count(p.failures),
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
pub(super) fn ActivityTrendChart(
    points: Vec<ActivityTrendPoint>,
    metric: Metric,
    #[props(default)] bucket_seconds: Option<u64>,
) -> Element {
    let (seconds, points) = trend_buckets(points, bucket_seconds);
    let time = bucket_bounds(&points, seconds);
    let items = points
        .into_iter()
        .map(|p| {
            let tokens = p.input_tokens.saturating_add(p.output_tokens);
            BarItem {
                window: bucket_window(p.bucket_start, seconds),
                value: metric.value(p.requests, tokens, p.api_equivalent_cost_usd, p.cache_hit_rate_observed),
                requests: p.requests,
                failures: p.failures,
                tokens,
                cache_read: p.cache_read_tokens,
                cache_hit_rate: p.cache_hit_rate_observed,
                cost_usd: p.api_equivalent_cost_usd,
            }
        })
        .collect();
    rsx! { InteractiveBars { label: "Activity over time (UTC)", metric, items, time } }
}

#[component]
pub(super) fn CockpitActivitySparkChart(overview: ActivityOverview, metric: String) -> Element {
    let mut active = use_signal(|| None::<usize>);
    let metric = Metric::parse(&metric);
    let (seconds, points) = trend_buckets(overview.trend, overview.bucket_seconds);
    let Some(view) = bucket_bounds(&points, seconds) else {
        return rsx! { p { class: "form-help", "{NO_RANGE_DATA}" } };
    };
    let plot = TimePlotBox { left: 2.0, right: 358.0, top: 16.0, bottom: 82.0 };
    let cache = TimePlotBox { top: 100.0, bottom: 138.0, ..plot };
    let span = TimeDelta::seconds(seconds as i64);
    let bucket_width = plot.x(view, view.start + span) - plot.left;
    let bar_width = (bucket_width * 0.56).min(28.0);
    let x = |point: &ActivityTrendPoint| plot.x(view, point.bucket_start + span / 2);
    let rate = |point: &ActivityTrendPoint| {
        point
            .cache_hit_rate_observed
            .filter(|p| p.is_finite())
            .or_else(|| effective_cache_hit_rate(point.input_tokens, point.cache_read_tokens, point.cache_hit_rate))
    };
    let value = |point: &ActivityTrendPoint| {
        metric
            .value(point.requests, point.input_tokens.saturating_add(point.output_tokens), point.api_equivalent_cost_usd, rate(point))
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    let peak = points.iter().filter_map(value).fold(0.0_f64, f64::max);
    let scale = if peak > 0.0 { peak } else { 1.0 };
    let count = points.len();
    let latest_rate = rate(&points[count - 1]);
    let active_index = active().filter(|&i| i < count);
    let selected = active_index.and_then(|i| points.get(i));
    let unit = metric.label();
    let bar_label = match metric {
        Metric::Cost => "API-EQ COST ($)",
        Metric::Tokens => "TOKENS",
        Metric::Requests => "REQUESTS",
        Metric::CacheHitRate => "CACHE HIT %",
    };
    let tick_stride = (42.0 / bucket_width).ceil().max(1.0) as usize;
    rsx! {
        div { class: "activity-plot cockpit-spark-plot", tabindex: "0", role: "group",
            aria_label: "{unit} and cache hit rate. Use left and right arrows to inspect.",
            onkeydown: move |e| step_bucket(e, active, count),
            onblur: move |_| active.set(None),
            onmouseleave: move |_| active.set(None),
            svg { class: "cockpit-spark-svg mono", width: "100%", view_box: "0 0 360 158", "aria-hidden": "true",
                text { class: "cockpit-spark-axis", x: "{plot.left}", y: "12", "{bar_label}" }
                text { class: "cockpit-spark-peak", x: "{plot.right}", y: "12", text_anchor: "end", "{fmt_compact_axis(metric, peak)}" }
                text { class: "cockpit-spark-axis", x: "{plot.left}", y: "96", "CACHE HIT %" }
                text { class: "cockpit-spark-cache", x: "{plot.right}", y: "96", text_anchor: "end", "{fmt_hit_pct(latest_rate)}" }
                for y in [chart_y(plot, 0.5), plot.bottom, chart_y(cache, 0.5), cache.bottom] {
                    line { x1: "{plot.left}", x2: "{plot.right}", y1: "{y}", y2: "{y}", class: "chart-gridline subtle" }
                }
                for (i, point) in points.iter().enumerate() {
                    if let Some(amount) = value(point).filter(|&v| v > 0.0) {
                        {
                            let y = chart_y(plot, amount / scale);
                            rsx! {
                                rect { key: "{i}", x: "{x(point) - bar_width / 2.0}", y: "{y}", rx: "2",
                                    width: "{bar_width}", height: "{plot.bottom - y}",
                                    class: if amount == peak || active_index == Some(i) { "activity-bar peak" } else { "activity-bar" } }
                            }
                        }
                    }
                }
                for (i, run) in points
                    .chunk_by(|a, b| rate(a).is_some() && rate(b).is_some() && b.bucket_start - a.bucket_start == span)
                    .enumerate()
                {
                    {
                        let coordinates = svg_points(run.iter().filter_map(|p| Some((x(p), chart_y(cache, rate(p)?)))));
                        rsx! {
                            if !coordinates.is_empty() {
                                g { key: "{i}",
                                    if run.len() > 1 {
                                        polygon { points: "{coordinates} {x(&run[run.len() - 1])},{cache.bottom} {x(&run[0])},{cache.bottom}",
                                            fill: "var(--ok)", opacity: "0.14" }
                                        polyline { points: coordinates, class: "chart-line chart-cache-line", fill: "none" }
                                    }
                                    if let Some(last) = run.last() && let Some(ratio) = rate(last) {
                                        circle { cx: "{x(last)}", cy: "{chart_y(cache, ratio)}", r: "3", class: "chart-cache-dot" }
                                    }
                                }
                            }
                        }
                    }
                }
                if seconds == 86400 {
                    for point in points.iter().step_by(tick_stride) {
                        text { key: "{point.bucket_start.timestamp()}", class: "cockpit-spark-axis", x: "{x(point).clamp(16.0, 344.0)}", y: "153", text_anchor: "middle",
                            {point.bucket_start.format("%b %d").to_string()} }
                    }
                } else {
                    TimeAxis { plot: cache, view, compact: true, label_offset: 15.0 }
                }
                if let Some(point) = selected {
                    TimeCrosshair { plot: TimePlotBox { bottom: cache.bottom, ..plot }, x: x(point) }
                }
                for (i, point) in points.iter().enumerate() {
                    rect { key: "{i}", class: "chart-hit", x: "{x(point) - bucket_width / 2.0}", y: "{plot.top}",
                        width: "{bucket_width}", height: "{cache.bottom - plot.top + 12.0}", fill: "transparent",
                        onmouseenter: move |_| active.set(Some(i)) }
                }
            }
            div { role: "status",
                if let Some(point) = selected {
                    ActivityReadout { right: x(point) < 180.0, heading: bucket_window(point.bucket_start, seconds),
                        fields: vec![(unit, metric.format_value(value(point))), ("Cache hit", fmt_hit_pct(rate(point)))] }
                }
            }
        }
    }
}

fn local_hour(hour: u8) -> String {
    format!("{} {}", if hour.is_multiple_of(12) { 12 } else { hour % 12 }, if hour < 12 { "AM" } else { "PM" })
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
                    window: local_hour(h),
                    value: metric.value(p.requests, p.tokens, p.api_equivalent_cost_usd, cache_hit_rate),
                    requests: p.requests,
                    failures: p.failures,
                    tokens: p.tokens,
                    cache_read: p.cache_read_tokens,
                    cache_hit_rate,
                    cost_usd: p.api_equivalent_cost_usd,
                }
            }
            None => BarItem {
                window: local_hour(h),
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
    rsx! { InteractiveBars { label: "By local hour", metric, items, rhythm: true } }
}

#[component]
pub(super) fn CacheHitRateChart(points: Vec<ActivityTrendPoint>, #[props(default)] bucket_seconds: Option<u64>) -> Element {
    let mut active = use_signal(|| None::<usize>);
    let measure = use_time_plot_measure();
    let (seconds, points) = trend_buckets(points, bucket_seconds);
    let Some(view) = bucket_bounds(&points, seconds) else {
        return rsx! { p { class: "form-help", "{NO_RANGE_DATA}" } };
    };
    let count = points.len();
    let width = (measure.width)().max(100.0);
    let plot = TimePlotBox { bottom: 118.0, ..activity_plot(width) };
    let volume_top = plot.bottom + 22.0;
    let volume = TimePlotBox { top: volume_top, bottom: volume_top + 44.0, ..plot };
    let inspection = TimePlotBox { bottom: volume.bottom, ..plot };
    let height = volume.bottom + 24.0;
    let step = (plot.right - plot.left) / count.max(1) as f64;
    let px = |i: usize| plot.left + (i as f64 + 0.5) * step;
    let max_input = points.iter().map(|p| p.input_tokens).max().unwrap_or_default() as f64;
    let volume_peak = fmt_compact_axis(Metric::Tokens, max_input);
    let observed = active().filter(|&i| i < count);
    rsx! {
            figure { class: "provider-chart chart-stage",
                figcaption { class: "chart-header",
                    span { "Cache hit rate (%) · UTC" }
                    span { class: "chart-inspect form-help", "{INSPECT}" }
                }
                if !points.iter().any(|p| p.cache_hit_rate_observed.is_some()) {
                    p { class: "form-help", "No observed cache hit rate in this range." }
                }
                div { class: "activity-plot", tabindex: "0", role: "group",
                    aria_label: "Cache hit rate and input volume. Use left and right arrows to inspect. Data table follows.",
                    onkeydown: move |e| step_bucket(e, active, count),
                    onblur: move |_| active.set(None),
                    onmouseleave: move |_| active.set(None),
                    onmounted: move |e| measure.mount(e),
                    onresize: move |_| measure.refresh(),
                    svg { width: "100%", height: "{height}", view_box: "0 0 {width} {height}", preserve_aspect_ratio: "none", "aria-hidden": "true",
                        ActivityGrid { plot, ticks: vec!["0%".into(), "50%".into(), "100%".into()] }
                        TimeAxis { plot: volume, view, label_offset: 17.0 }
                        line { x1: "{volume.left}", x2: "{volume.right}", y1: "{volume.bottom}", y2: "{volume.bottom}", class: "chart-gridline" }
                        for (i, p) in points.iter().enumerate().filter(|(_, p)| p.input_tokens > 0) {
                            {
                                let bar_w = (step * 0.74).min(34.0);
                                let y = chart_y(volume, p.input_tokens as f64 / max_input);
                                rsx! { rect { x: "{px(i) - bar_w / 2.0}", y: "{y}", width: "{bar_w}", height: "{volume.bottom - y}", rx: "2", class: "activity-volume-bar" } }
                            }
                        }
                        for (i, pair) in points.windows(2).enumerate() {
                            if let Some((a, b)) = pair[0].cache_hit_rate_observed.zip(pair[1].cache_hit_rate_observed)
                                && pair[1].bucket_start - pair[0].bucket_start == TimeDelta::seconds(seconds as i64) {
                                line { x1: "{px(i)}", x2: "{px(i + 1)}", y1: "{chart_y(plot, a)}", y2: "{chart_y(plot, b)}",
                                    class: "chart-line chart-cache-line activity-cache-segment" }
                            }
                        }
                        if let Some(i) = observed { TimeCrosshair { plot: inspection, x: px(i) } }
                        text { x: "{volume.left}", y: "{volume.top - 5.0}", class: "activity-volume-label", "Input volume · peak {volume_peak} tok" }
                        for (i, p) in points.iter().enumerate() {
                            if let Some(rate) = p.cache_hit_rate_observed {
                                circle { cx: "{px(i)}", cy: "{chart_y(plot, rate)}", r: if observed == Some(i) { "4.5" } else { "2.6" },
                                    class: if observed == Some(i) { "chart-cache-dot active" } else { "chart-cache-dot" } }
                            }
                        }
                        for i in 0..count {
                            BucketHit { plot: inspection, index: i, count, active }
                        }
                    }
                    div { role: "status",
                        if let Some(i) = observed {
                            {
                                let p = &points[i];
                                rsx! {
                                    ActivityReadout { right: i < count / 2, heading: bucket_window(p.bucket_start, seconds), fields: vec![
                                        ("Cache hit", p.cache_hit_rate_observed.map_or_else(|| "N/A · no input".into(), |r| fmt_hit_pct(Some(r)))),
                                        ("Read / input tokens", format!("{} / {}", fmt_count(p.cache_read_tokens), fmt_count(p.input_tokens))),
                                        ("Cache write tokens", fmt_count(p.cache_write_tokens)),
                                    ] }
                                }
                            }
                        }
                    }
                }
                details { summary { "Data table" }
                    Table { label: "Cache hit rate by bucket", headers: &["Period", "Cache hit %", "Cache read", "Input tokens", "Cache write"],
                        rows: points.iter().map(|p| vec![
                            bucket_window(p.bucket_start, seconds), fmt_hit_pct(p.cache_hit_rate_observed),
                            fmt_count(p.cache_read_tokens), fmt_count(p.input_tokens), fmt_count(p.cache_write_tokens),
                        ]).collect()
                    }
                }
            }
    }
}

#[component]
fn HeatmapScale(metric: Metric, max: f64) -> Element {
    let peak = fmt_compact_axis(metric, max);
    let unit = match metric {
        Metric::Requests => " req",
        Metric::Tokens => " tok",
        _ => "",
    };
    rsx! {
        div { class: "activity-heat-scale", aria_label: "Heatmap color scale: zero to {peak}{unit}",
            span { class: "chart-heat-swatch zero", aria_hidden: "true" }
            span { class: "mono", "0" }
            span { "Low" }
            for step in [0.12, 0.36, 0.65, 1.0] {
                span { class: if metric == Metric::CacheHitRate { "chart-heat-swatch cache" } else { "chart-heat-swatch" },
                    style: "opacity: {step}", aria_hidden: "true" }
            }
            span { "Peak" }
            span { class: "mono", "{peak}{unit}" }
        }
    }
}

#[component]
pub(super) fn ActivityHeatmap(cells: Vec<HeatmapCell>, metric: Metric, #[props(default)] covered_weekdays: Vec<u8>) -> Element {
    let mut hovered = use_signal(|| None::<(usize, usize)>);
    let measure = use_time_plot_measure();
    let hatch = use_hook(|| format!("activity-outside-{}", Uuid::new_v4().simple()));
    let width = (measure.width)().max(100.0);
    let step = (width - 38.0) / 24.0;
    let in_scope = |day| covered_weekdays.is_empty() || covered_weekdays.contains(&(day as u8));
    if cells.is_empty() && covered_weekdays.is_empty() {
        return rsx! { p { class: "form-help", "{NO_RANGE_DATA}" } };
    }
    let unit = metric.label();
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let cell_map: BTreeMap<_, _> = cells
        .iter()
        .filter(|c| in_scope(usize::from(c.day_of_week)))
        .map(|c| {
            let rate = effective_cache_hit_rate(c.input_tokens, c.cache_read_tokens, c.cache_hit_rate);
            let value = metric.value(c.requests, c.tokens, c.api_equivalent_cost_usd, rate);
            ((usize::from(c.day_of_week), usize::from(c.hour)), (c, rate, value))
        })
        .collect();
    let max = cell_map.values().filter_map(|&(_, _, v)| v).fold(0.0, f64::max);
    let empty_value = metric.value(0, 0, 0.0, None);
    let value_at = |d, h| cell_map.get(&(d, h)).map_or(empty_value, |&(_, _, value)| value);
    let active = hovered();
    let cell_class = if metric == Metric::CacheHitRate { "chart-heat-cache" } else { "chart-bar chart-solid" };
    rsx! {
        figure { class: "provider-chart chart-stage",
            figcaption { class: "chart-header",
                span { "By weekday and local hour · {unit}" }
                HeatmapScale { metric, max }
            }
            div { class: "chart-legend",
                if !covered_weekdays.is_empty() && covered_weekdays.len() < days.len() {
                    span { class: "chart-legend-item", span { class: "chart-heat-swatch outside" } "Outside selected window" }
                }
                if metric == Metric::CacheHitRate {
                    span { class: "chart-legend-item", span { class: "chart-heat-swatch na" } "N/A · no input" }
                }
            }
            div { class: "activity-plot",
                onmounted: move |e| measure.mount(e),
                onresize: move |_| measure.refresh(),
                svg {
                width: "100%", height: "154", view_box: "0 0 {width} 154", preserve_aspect_ratio: "none",
                role: "img",
                "aria-label": "Seven by twenty-four activity heatmap. Data table follows.",
                onmouseleave: move |_| hovered.set(None),
                defs { pattern { id: "{hatch}", width: "5", height: "5", pattern_units: "userSpaceOnUse",
                    path { d: "M-1 1L1 -1 M0 5L5 0 M4 6L6 4", class: "activity-heat-hatch" }
                } }
                for hour in (0..24).step_by(if step >= 18.0 { 3 } else { 6 }) {
                    text {
                        x: "{35.0 + step * (hour as f64 + 0.5)}",
                        y: "12",
                        text_anchor: "middle",
                        class: if active.is_some_and(|(_, h)| h == hour) { "chart-axis-active" } else { "" },
                        "{hour:02}"
                    }
                }
                for (d, day) in days.into_iter().enumerate() {
                    text {
                        x: "2",
                        y: "{30 + 18 * d}",
                        class: if active.is_some_and(|(ad, _)| ad == d) { "chart-axis-active" } else { "" },
                        "{day}"
                    }
                    for h in 0..24 {
                        {
                            let value = value_at(d, h);
                            let is_active = active == Some((d, h));
                            let cx = 35.0 + step * h as f64;
                            let cy = 18 + 18 * d;
                            let outside = !in_scope(d);
                            let (state, opacity) = match (outside, value) {
                                (true, _) => ("activity-heat-outside", 1.0),
                                (false, None) => ("activity-heat-na", 1.0),
                                (false, Some(0.0)) => ("activity-heat-zero", 1.0),
                                (false, Some(intensity)) => (cell_class, 0.12 + 0.88 * (intensity / max).clamp(0.0, 1.0)),
                            };
                            let active_class = if is_active { "chart-cell-active" } else { "" };
                            let description = if outside { "Outside selected window".into() } else if value.is_none() {
                                "N/A · no input tokens".into()
                            } else { format!("{unit}: {}", metric.format_value(value)) };
                            rsx! {
                                rect {
                                    x: "{cx}",
                                    y: "{cy}",
                                    width: "{(step - 2.0).max(1.0)}",
                                    height: "16",
                                    rx: "2",
                                    class: "{state} {active_class} chart-hit",
                                    fill: if outside { format!("url(#{hatch})") } else { String::new() },
                                    opacity,
                                    onmouseenter: move |_| hovered.set(Some((d, h))),
                                    title { "{day} {h:02}:00 · {description}" }
                                }
                            }
                        }
                    }
                }
                if let Some((d, h)) = active {
                    {
                        let (detail, subdetail) = if !in_scope(d) {
                            ("Outside selected window".into(), None)
                        } else if value_at(d, h).is_none() {
                            ("N/A · no input tokens".into(), None)
                        } else if let Some(&(c, rate, _)) = cell_map.get(&(d, h)) {
                            (
                                format!("{} req · {} tok", fmt_count(c.requests), fmt_count(c.tokens)),
                                Some(format!("Cache {} · ${:.4}", fmt_hit_pct(rate), c.api_equivalent_cost_usd)),
                            )
                        } else {
                            ("No recorded requests".into(), None)
                        };
                        let spec = TooltipSpec {
                            x: 35.0 + step * (h as f64 + 0.5),
                            y: 26.0 + 18.0 * d as f64,
                            crosshair_x: None,
                            y_top: 20.0,
                            y_bottom: 144.0,
                            view_w: width,
                            title: if in_scope(d) && value_at(d, h).is_some() {
                                format!("{} {h:02}:00 · {}", days[d], metric.format_value(value_at(d, h)))
                            } else { format!("{} {h:02}:00", days[d]) },
                            detail,
                            subdetail,
                        };
                        rsx! { SvgTooltip { spec } }
                    }
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

fn drift_date(date: &str) -> Option<DateTime<Utc>> {
    Some(chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?.and_time(chrono::NaiveTime::MIN).and_utc())
}

fn drift_ticks(peak: f64) -> [f64; 3] {
    let target = (peak / 2.0).max(1.0);
    let decade = 10.0_f64.powf(target.log10().floor());
    let step = decade * [1.0, 2.0, 2.5, 5.0, 10.0].into_iter().find(|n| *n >= target / decade).unwrap_or(10.0);
    [0.0, step, 2.0 * step]
}

#[component]
pub(super) fn DriftBandChart(days: Vec<DriftDayPoint>, markers: Vec<DriftMarker>, #[props(default)] ttft: bool) -> Element {
    let mut active = use_signal(|| None::<usize>);
    let measure = use_time_plot_measure();
    let mut dated: Vec<_> = days.iter().filter_map(|day| Some((drift_date(&day.date)?, day))).collect();
    dated.sort_by_key(|(at, _)| *at);
    let Some(((first, _), (last, _))) = dated.first().zip(dated.last()) else {
        return rsx! { p { class: "form-help", "No dated observations in this scope." } };
    };
    let view = TimeBounds { start: *first - TimeDelta::hours(12), end: *last + TimeDelta::hours(12) };
    let width = (measure.width)().max(100.0);
    let plot = activity_plot(width);
    let height = plot.bottom + 24.0;
    let x = |at| plot.x(view, at);
    let value =
        |d: &DriftDayPoint| (if ttft { d.median_ttft_ms } else { Some(d.thinking_per_reply) }).filter(|v| v.is_finite() && *v >= 0.0);
    let peak = dated.iter().filter_map(|&(at, d)| Some((at, value(d)?))).max_by(|a, b| a.1.total_cmp(&b.1));
    let high = dated.iter().filter_map(|(_, d)| d.baseline_high.filter(|v| v.is_finite())).fold(0.0, f64::max);
    let ticks = drift_ticks(peak.map_or(0.0, |(_, v)| v).max(if ttft { 0.0 } else { high }));
    let y = |v: f64| chart_y(plot, v / ticks[2]);
    let half_day = (x(*first + TimeDelta::hours(12)) - x(*first) - 0.5).clamp(0.0, 15.0);
    let show_median = !ttft && dated.iter().any(|(_, d)| d.baseline_median_thinking.is_some());
    let show_band = !ttft && dated.iter().any(|(_, d)| d.baseline_low.zip(d.baseline_high).is_some());
    let show_outlier = !ttft && dated.iter().any(|(_, d)| d.outlier);
    let markers: Vec<_> = markers.iter().filter_map(|m| Some((drift_date(&m.date)?, m))).filter(|(at, _)| view.contains(*at)).collect();
    let targets: Vec<_> = markers
        .iter()
        .map(|&(at, _)| (at, None))
        .chain(dated.iter().map(|&(at, day)| (at, Some(day))))
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect();
    let count = targets.len();
    let selected = active().filter(|&i| i < count);
    let label = if ttft { "Daily median TTFT (ms)" } else { "Daily thinking tokens per reply" };
    rsx! {
        figure { class: "provider-chart chart-stage",
            figcaption { class: "chart-header",
                span { "{label} · UTC" }
                span { class: "chart-inspect form-help", "{INSPECT}" }
            }
            div { class: "chart-legend",
                span { class: "chart-legend-item", span { class: "chart-swatch" } "Observed" }
                if show_median { span { class: "chart-legend-item", span { class: "chart-swatch drift-median" } "14d median" } }
                if show_band { span { class: "chart-legend-item", span { class: "chart-swatch band" } "Normal band" } }
                if !markers.is_empty() { span { class: "chart-legend-item", span { class: "chart-swatch reset" } "Client change" } }
                if show_outlier { span { class: "chart-legend-item", span { class: "chart-swatch outlier" } "Outlier" } }
            }
            if ttft && !dated.iter().any(|(_, d)| value(d).is_some()) {
                p { class: "form-help", "No observed TTFT in this scope." }
            }
            div { class: "activity-plot", tabindex: "0", role: "group",
                aria_label: "{label}. Use left and right arrows to inspect. Daily data table follows.",
                onkeydown: move |e| step_bucket(e, active, count),
                onblur: move |_| active.set(None),
                onmouseleave: move |_| active.set(None),
                onmounted: move |e| measure.mount(e),
                onresize: move |_| measure.refresh(),
                svg { width: "100%", height: "{height}", view_box: "0 0 {width} {height}", preserve_aspect_ratio: "none", "aria-hidden": "true",
                    TimeAxis { plot, view, date_only: true }
                    ActivityGrid { plot, ticks: ticks.map(|tick| if tick.fract() != 0.0 { format!("{tick:.1}") } else { fmt_compact_axis(Metric::Tokens, tick) }).to_vec() }
                    if !ttft {
                        for (at, day) in dated.iter().copied() {
                            if let Some((low, high)) = day.baseline_low.zip(day.baseline_high) {
                                rect { x: "{x(at) - half_day}", width: "{half_day * 2.0}", y: "{y(high)}", height: "{(y(low) - y(high)).max(1.0)}", class: "chart-band" }
                            }
                            if let Some(median) = day.baseline_median_thinking {
                                line { x1: "{x(at) - half_day}", x2: "{x(at) + half_day}", y1: "{y(median)}", y2: "{y(median)}", class: "chart-median" }
                            }
                        }
                    }
                    for (at, marker) in &markers {
                        line { x1: "{x(*at)}", x2: "{x(*at)}", y1: "{plot.top - 6.0}", y2: "{plot.bottom}", class: "chart-reset", title { "{marker.label}" } }
                        circle { cx: "{x(*at)}", cy: "{plot.top - 6.0}", r: "2.5", class: "chart-marker-dot" }
                    }
                    for pair in dated.windows(2) {
                        if let Some((a, b)) = value(pair[0].1).zip(value(pair[1].1)) && pair[1].0 - pair[0].0 == TimeDelta::days(1) {
                            line { x1: "{x(pair[0].0)}", x2: "{x(pair[1].0)}", y1: "{y(a)}", y2: "{y(b)}", class: "chart-line chart-drift-line" }
                        }
                    }
                    for (i, (at, day)) in targets.iter().copied().enumerate() {
                        {
                            let cx = x(at);
                            let left = x(at - TimeDelta::hours(12));
                            let right = x(at + TimeDelta::hours(12));
                            let outlier = !ttft && day.is_some_and(|d| d.outlier);
                            rsx! {
                                if selected == Some(i) { TimeCrosshair { plot, x: cx } }
                                if let Some(v) = day.and_then(value) {
                                    circle { cx: "{cx}", cy: "{y(v)}", r: if selected == Some(i) || outlier { "4.5" } else { "2.8" },
                                        class: if outlier { "chart-outlier" } else { "chart-solid" } }
                                }
                                rect { x: "{left}", y: "{plot.top - 9.0}", width: "{right - left}", height: "{plot.bottom - plot.top + 29.0}",
                                    fill: "transparent", class: "chart-hit", onmouseenter: move |_| active.set(Some(i)),
                                    onmouseleave: move |_| active.set(None) }
                            }
                        }
                    }
                    if let Some((at, peak)) = peak {
                        {
                            let cx = x(at);
                            let cy = y(peak);
                            let align_right = cx > (plot.left + plot.right) / 2.0;
                            let label_x = if align_right { cx - 6.0 } else { cx + 6.0 };
                            let label_y = cy - 6.0;
                            rsx! { text { class: "activity-peak-label", pointer_events: "none", x: "{label_x}", y: "{label_y}",
                                text_anchor: if align_right { "end" } else { "start" }, "Peak {peak:.1}" } }
                        }
                    }
                }
                div { role: "status",
                    if let Some(i) = selected {
                        {
                            let (at, day) = targets[i];
                            let mut fields = Vec::new();
                            if let Some(d) = day {
                                let observed = value(d).map_or_else(|| "Not observed".into(), |v| if ttft { format!("{v:.1} ms") } else { format!("{v:.1} tokens/reply") });
                                fields.extend([("Observed", observed), ("Replies", fmt_count(d.replies))]);
                                if !ttft {
                                    if let Some(median) = d.baseline_median_thinking { fields.push(("14d median", format!("{median:.1}"))); }
                                    if let Some((low, high)) = d.baseline_low.zip(d.baseline_high) { fields.push(("Normal band", format!("{low:.1}–{high:.1}"))); }
                                    let judgment = if d.outlier { "Outlier" } else if d.judgeable { "Not an outlier" } else { "Insufficient coverage" };
                                    fields.push(("Judgment", judgment.into()));
                                }
                            }
                            for (_, marker) in markers.iter().filter(|(time, _)| *time == at) { fields.push(("Client change", marker.label.clone())); }
                            rsx! { ActivityReadout { right: view.fraction(at) < 0.5, heading: at.format("%Y-%m-%d").to_string(), fields } }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use dioxus::dioxus_core::{AttributeValue, Mutation, Mutations};

    use super::*;

    fn unified_report() -> ProviderQuotaReport {
        use serde_json::json;
        let at = DateTime::UNIX_EPOCH + TimeDelta::days(1);
        let sample = |account: &str, remaining: f64, state: &str, offset: i64| {
            let observed = at + TimeDelta::minutes(offset);
            json!({
                "id": offset, "provider_id": "fixture", "account_id": account, "window_key": "gemini-weekly",
                "label": "Gemini Weekly", "kind": "weekly", "unit": "percent", "remaining_percent": remaining,
                "cadence": "unknown", "status": "allowed", "observation_state": state,
                "observed_at": observed, "recorded_at": observed, "stale": false
            })
        };
        serde_json::from_value(json!({
            "provider": {"id": "fixture", "label": "Fixture", "family": "fixture", "mode": "fixture", "verification": "built"},
            "as_of": at + TimeDelta::minutes(10),
            "accounts": [
                {"id": "ultra", "provider_id": "fixture", "source_account_id": "ultra", "display_name": "Ultra",
                 "plan": "Ultra", "tier_multiplier": 20},
                {"id": "pro", "provider_id": "fixture", "source_account_id": "pro", "display_name": "Pro", "plan": "Pro"},
                {"id": "missing", "provider_id": "fixture", "source_account_id": "missing", "display_name": "Missing", "plan": "Pro"}
            ],
            "pools": [{
                "provider_id": "fixture", "window_key": "gemini-weekly", "label": "Gemini Weekly", "kind": "weekly",
                "eligible_accounts": 3, "included_accounts": 2, "capacity_basis": "plan_multiplier", "is_estimate": false
            }, {
                "provider_id": "fixture", "window_key": "3p-5h", "label": "3P 5h", "kind": "session",
                "eligible_accounts": 3, "included_accounts": 1, "capacity_basis": "plan_multiplier", "is_estimate": false,
                "weighted_remaining_percent": 71.0
            }],
            "samples": [
                sample("pro", 50.0, "measured", 10),
                sample("missing", 0.0, "unavailable", 5),
                sample("ultra", 25.0, "measured", 0)
            ],
            "past_windows": [], "forecasts": []
        }))
        .unwrap()
    }

    #[test]
    fn compact_window_pill_label_shortens_known_windows() {
        for (label, key, expected) in [
            ("Gemini Weekly", "gemini-weekly", "Gem Wk"),
            ("Gemini 5h", "gemini-5h", "Gem 5h"),
            ("3P Weekly", "3p-weekly", "3P Wk"),
            ("3P 5h", "3p-5h", "3P 5h"),
            ("  Daily  ", "daily", "Daily"),
        ] {
            assert_eq!(compact_window_pill_label(label, key), expected);
        }
    }

    #[test]
    fn unified_pool_series_aggregates_tier_weighted_windows_without_zeroing_missing() {
        let mut report = unified_report();
        let series = build_unified_pool_series(&report);
        assert_eq!(series.len(), 2);
        let points = &series[0].points;
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].remaining_percent, 25.0);
        assert_eq!(points[1].remaining_capacity_units, Some(5.5));
        assert_eq!(points[1].total_capacity_units, Some(21.0));
        assert!((points[1].remaining_percent - 550.0 / 21.0).abs() < 1e-8);
        assert_eq!(series[1].points[0].at, report.as_of);
        assert_eq!(series[1].points[0].remaining_percent, 71.0);

        // Do not carry old readings beyond the reset grace period.
        report.samples[2].resets_at = Some(report.samples[2].observed_at);
        report.samples[0].observed_at += TimeDelta::minutes(6);
        report.as_of = report.samples[0].observed_at;
        assert_eq!(build_unified_pool_series(&report)[0].points[1].remaining_percent, 50.0);
    }

    #[test]
    fn tier_split_series_stacks_heterogeneous_tiers_and_returns_none_for_uniform_pool() {
        let mut report = unified_report();
        let projected = report.as_of + TimeDelta::hours(1);
        report.forecasts.push(QuotaForecast {
            window_key: "gemini-weekly".into(),
            account_id: None,
            scope_model: None,
            drain_pct_per_hour: 1.0,
            projected_exhaustion_at: None,
            resets_before_exhaustion: false,
            lookback_hours: 6,
            horizon_hours: 12,
            points: vec![ForecastPoint { at: projected, used_percent: 75.0, remaining_percent: 25.0 }],
            is_estimate: true,
        });
        let split = build_tier_split_series(&report, "gemini-weekly", None).unwrap();
        assert_eq!(split.total_capacity_units, 22.0);
        assert_eq!(split.bands[0].tier_label, "20× Ultra");
        assert_eq!(split.bands[1].tier_label, "1× Pro");
        assert_eq!(split.bands[0].points[1], TierBandPoint { at: report.as_of, y0: 0.0, y1: 5.0, observed: true });
        assert_eq!(split.bands[1].points[1], TierBandPoint { at: report.as_of, y0: 5.0, y1: 5.5, observed: true });
        assert_eq!(split.total_points[1], (report.as_of, 5.5));
        // Only 21x (20x Ultra + 1x Pro) is observed at NOW; forecast scales to observed capacity to avoid a jump.
        assert_eq!(split.forecast_units, [(projected, 5.25)]);
        report.samples.clear();
        report.accounts[0].windows = serde_json::from_value(serde_json::json!([{
            "window_key": "gemini-weekly", "label": "Gemini Weekly",
            "remaining_percent": 30.0, "observation_state": "measured"
        }]))
        .unwrap();
        let current_only = build_tier_split_series(&report, "gemini-weekly", None).unwrap();
        assert_eq!(current_only.total_points, [(report.as_of, 6.0)]);
        report.accounts[0].tier_multiplier = None;
        assert!(build_tier_split_series(&report, "gemini-weekly", None).is_none());
    }

    #[test]
    fn tier_split_distinguishes_unobserved_tier_from_measured_zero() {
        let mut report = unified_report();
        // At t0 (ultra only): 1x Pro tier is unobserved; at t1 (as_of): 1x Pro is measured at 0%.
        report.samples[0].remaining_percent = Some(0.0);
        let split = build_tier_split_series(&report, "gemini-weekly", None).unwrap();
        let pro = &split.bands[1];
        assert_eq!(pro.tier_label, "1× Pro");
        assert!(!pro.points[0].observed);
        assert_eq!(pro.points[0].y1 - pro.points[0].y0, 0.0);
        assert!(pro.points[1].observed);
        assert_eq!(pro.points[1].y1 - pro.points[1].y0, 0.0);
    }

    #[test]
    fn unified_pool_series_excludes_disabled_accounts_and_falls_back_to_used_percent() {
        let mut report = unified_report();
        // Disable the 20x Ultra account so only enabled 1x Pro (50%) contributes.
        report.accounts[0].enabled = false;
        report.accounts[1].enabled = true;
        report.accounts[2].enabled = true;
        // Second pool summary only populates weighted_used_percent.
        report.pools[1].weighted_remaining_percent = None;
        report.pools[1].weighted_used_percent = Some(29.0);

        let series = build_unified_pool_series(&report);
        assert_eq!(series[0].points.len(), 1);
        assert_eq!(series[0].points[0].remaining_percent, 50.0);
        assert_eq!(series[0].points[0].total_capacity_units, Some(1.0));
        assert_eq!(series[1].current_remaining_percent, Some(71.0));
    }

    #[test]
    fn interpolated_projection_at_interpolates_between_forecast_points() {
        let start = DateTime::UNIX_EPOCH;
        let points = vec![(start, 80.0), (start + TimeDelta::hours(2), 40.0)];
        assert_eq!(interpolated_projection_at(&points, start), Some(80.0));
        assert_eq!(interpolated_projection_at(&points, start + TimeDelta::hours(1)), Some(60.0));
        assert_eq!(interpolated_projection_at(&points, start + TimeDelta::hours(2)), Some(40.0));
        assert_eq!(interpolated_projection_at(&points, start + TimeDelta::hours(3)), None);
    }

    #[test]
    fn unified_pool_quota_chart_renders_single_svg_with_tier_split_toggle() {
        use dioxus::html::{PlatformEventData, SerializedHtmlEventConverter, SerializedMouseData, set_event_converter};

        fn app() -> Element {
            let range_hours = use_signal(|| 24);
            let show_forecast = use_signal(|| true);
            rsx! { UnifiedPoolQuotaChart { report: unified_report(), range_hours, show_forecast } }
        }
        let mut dom = VirtualDom::new(app);
        let mutations = dom.rebuild_to_vec();
        assert_eq!(svg_attribute_values(&mutations, "viewBox").len(), 1);
        let rendered = format!("{mutations:?}");
        for label in ["Gem Wk", "3P 5h", "Tier split (22×)", "No forecast"] {
            assert!(rendered.contains(label), "{label} missing from {rendered}");
        }
        let mut id = None;
        let tier_toggle = mutations
            .edits
            .iter()
            .filter_map(|mutation| match mutation {
                Mutation::PushId { id: current } | Mutation::SetId { id: current } => {
                    id = Some(*current);
                    None
                }
                Mutation::SetAttribute { name, value: AttributeValue::Text(value), .. }
                    if name == "title" && value == "Tier split (22×)" =>
                {
                    id
                }
                _ => None,
            })
            .next()
            .expect("tier split button");
        set_event_converter(Box::new(SerializedHtmlEventConverter));
        let data = PlatformEventData::new(Box::<SerializedMouseData>::default());
        dom.runtime().handle_event("click", Event::new(Rc::new(data), true).into_any(), tier_toggle);
        let rendered = format!("{:?}", dom.render_immediate_to_vec());
        assert!(rendered.contains("20× Ultra"), "{rendered}");
        assert!(rendered.contains("5.0 / 20.0×"), "{rendered}");
    }

    fn svg_attribute_values<'a>(rendered: &'a Mutations, attr: &str) -> Vec<&'a str> {
        rendered
            .edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::SetAttribute { name, value: AttributeValue::Text(value), .. } if name == attr => Some(value.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn time_axes_label_midnight_and_keep_the_closing_date() {
        let start = DateTime::UNIX_EPOCH + TimeDelta::hours(10);
        let view = TimeBounds { start, end: start + TimeDelta::days(1) };
        assert_eq!(bucket_badge(Some(3600)), Some("Hourly · 24h"));
        assert_eq!(bucket_badge(Some(10800)), Some("3-hour · 7d"));
        assert_eq!(bucket_badge(Some(86400)), Some("Daily"));
        let ticks = time_ticks(view, 260.0, true, false);
        assert_eq!(ticks.len(), 3);
        assert_eq!(ticks[0], (view.start, "Jan 01 10:00".into()));
        assert_eq!(ticks[1], (DateTime::UNIX_EPOCH + TimeDelta::days(1), "Jan 02 00:00".into()));
        assert_eq!(ticks[2], (view.end, "10:00".into()));
        assert_eq!(
            time_ticks(view, 1000.0, false, false).into_iter().map(|(_, label)| label).collect::<Vec<_>>(),
            ["Jan 01 10:00", "16:00", "Jan 02 00:00", "04:00", "10:00"]
        );
        let wide = TimeBounds { start, end: start + TimeDelta::days(7) };
        let dates = time_ticks(wide, 300.0, false, true);
        assert!((4..=5).contains(&dates.len()));
        assert_eq!(dates.last(), Some(&(wide.end, "Jan 08".into())));
    }

    #[test]
    fn hourly_ticks_snap_interiors_and_hide_a_fractional_label_near_midnight() {
        let midnight = DateTime::UNIX_EPOCH + TimeDelta::days(1);
        let start = midnight - TimeDelta::hours(10) + TimeDelta::minutes(15);
        let view = TimeBounds { start, end: start + TimeDelta::days(1) };
        let ticks = time_ticks(view, 1000.0, false, false);
        assert_eq!(ticks[0], (view.start, "Jan 01 14:15".into()));
        assert_eq!(ticks[4], (view.end, "14:15".into()));
        assert!(ticks[1..4].iter().all(|(at, _)| at.timestamp().rem_euclid(3600) == 0));
        assert!(ticks.contains(&(midnight, "Jan 02 00:00".into())));

        let start = midnight - TimeDelta::minutes(15);
        let view = TimeBounds { start, end: start + TimeDelta::days(1) };
        let ticks = time_ticks(view, 260.0, true, false);
        assert_eq!(ticks, [(view.start, String::new()), (midnight, "Jan 02 00:00".into()), (view.end, "23:45".into())]);
        let end = midnight + TimeDelta::minutes(15);
        let view = TimeBounds { start: end - TimeDelta::hours(48), end };
        let ticks = time_ticks(view, 1000.0, false, false);
        assert_eq!(ticks.len(), 5);
        assert_eq!(ticks[4], (view.end, String::new()));
        assert!(ticks[1..4].iter().all(|(at, _)| at.timestamp().rem_euclid(3600) == 0));
        assert!(ticks.contains(&(midnight, "Jan 02 00:00".into())));
    }

    #[test]
    fn snapped_midnight_gridline_stays_exact_and_label_faces_inward() {
        const PLOT: TimePlotBox = TimePlotBox { left: 48.0, right: 1048.0, top: 8.0, bottom: 150.0 };
        fn view() -> TimeBounds {
            let end = DateTime::UNIX_EPOCH + TimeDelta::days(1) + TimeDelta::minutes(15);
            TimeBounds { start: end - TimeDelta::hours(48), end }
        }
        fn app() -> Element {
            rsx! { TimeAxis { plot: PLOT, view: view(), grid: true } }
        }
        let mutations = VirtualDom::new(app).rebuild_to_vec();
        let midnight = midnight(view().end);
        let grid_x = PLOT.x(view(), midnight).to_string();
        assert!(svg_attribute_values(&mutations, "x1").contains(&grid_x.as_str()));
        assert!(svg_attribute_values(&mutations, "x2").contains(&grid_x.as_str()));

        let positions = svg_attribute_values(&mutations, "x");
        let anchors = svg_attribute_values(&mutations, "text-anchor");
        let i = positions.iter().position(|&x| x == grid_x).unwrap();
        assert_eq!(anchors[i], "end");
        let label_right = positions[i].parse::<f64>().unwrap();
        let label_left = label_right - "Jan 02 00:00".len() as f64 * 6.7;
        assert!(label_left >= PLOT.left && label_right <= PLOT.right);
    }

    #[test]
    fn quota_pool_defaults_to_multiples_only_above_six_series() {
        assert!(!quota_defaults_to_multiples(true, 6));
        assert!(quota_defaults_to_multiples(true, 7));
        assert!(!quota_defaults_to_multiples(false, 12));
    }

    #[test]
    fn drift_ticks_cover_the_data_with_round_intervals() {
        assert_eq!(drift_ticks(1884.0), [0.0, 1000.0, 2000.0]);
        assert_eq!(drift_ticks(42.0), [0.0, 25.0, 50.0]);
        assert_eq!(drift_ticks(0.0), [0.0, 1.0, 2.0]);
    }

    #[test]
    fn adaptive_cache_preserves_none_and_scales_volume_independently() {
        fn app() -> Element {
            let mut points = vec![daily_point(0, 100), daily_point(1, 0), daily_point(2, 200)];
            points[0].cache_hit_rate_observed = Some(0.6);
            points[1].cache_read_tokens = 0;
            points[2].cache_hit_rate_observed = Some(0.0);
            rsx! { CacheHitRateChart { points, bucket_seconds: Some(86400) } }
        }
        let mutations = VirtualDom::new(app).rebuild_to_vec();
        let heights = svg_attribute_values(&mutations, "height");
        assert!(heights.contains(&"22")); // Half the input of the highest-volume bucket.
        assert!(heights.contains(&"44"));
        assert!(!heights.contains(&"0")); // No empty volume rectangle for the unobserved bucket.
        let rendered = format!("{mutations:?}");
        assert!(rendered.contains("activity-volume-bar"));
        assert_eq!(rendered.matches("chart-cache-dot").count(), 2);
        assert!(rendered.contains("60.0%"));
        assert!(rendered.contains("0.0%"));
        assert!(rendered.contains("—"));
        assert!(!rendered.contains("activity-cache-segment"));
    }

    #[test]
    fn heatmap_distinguishes_zero_low_missing_and_outside_cells() {
        fn app() -> Element {
            let zero = HeatmapCell {
                day_of_week: 0,
                hour: 0,
                requests: 1,
                tokens: 100,
                input_tokens: 100,
                cache_read_tokens: 0,
                cache_hit_rate: 0.0,
                api_equivalent_cost_usd: 0.0,
            };
            let low = HeatmapCell { hour: 1, cache_read_tokens: 1, cache_hit_rate: 0.01, ..zero.clone() };
            let peak = HeatmapCell { hour: 2, cache_read_tokens: 100, cache_hit_rate: 1.0, ..zero.clone() };
            rsx! { ActivityHeatmap { cells: vec![zero, low, peak], metric: Metric::CacheHitRate, covered_weekdays: vec![0] } }
        }
        let mutations = VirtualDom::new(app).rebuild_to_vec();
        let classes: Vec<_> = svg_attribute_values(&mutations, "class").into_iter().filter(|class| class.ends_with(" chart-hit")).collect();
        let opacities: Vec<_> = mutations
            .edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::SetAttribute { name, value: AttributeValue::Float(opacity), .. } if name == "opacity" => Some(*opacity),
                _ => None,
            })
            .collect();
        assert_eq!(classes.len(), 7 * 24);
        assert_eq!(opacities.len(), classes.len());
        let cell = |day: usize, hour: usize| (classes[day * 24 + hour], opacities[day * 24 + hour]);
        assert_eq!(cell(0, 0), ("activity-heat-zero  chart-hit", 1.0));
        let (low_class, low_opacity) = cell(0, 1);
        assert_eq!(low_class, "chart-heat-cache  chart-hit");
        assert!((low_opacity - 0.1288).abs() < 1e-12);
        let (peak_class, peak_opacity) = cell(0, 2);
        assert_eq!((peak_class, peak_opacity), ("chart-heat-cache  chart-hit", 1.0));
        assert!(peak_opacity > low_opacity);
        assert_eq!(cell(0, 3), ("activity-heat-na  chart-hit", 1.0));
        assert_eq!(cell(1, 0), ("activity-heat-outside  chart-hit", 1.0));
    }

    #[test]
    fn drift_does_not_connect_calendar_gaps_or_apply_thinking_baseline_to_ttft() {
        fn app() -> Element {
            let day = |date: &str, median_ttft_ms| DriftDayPoint {
                date: date.into(),
                replies: 30,
                thinking_per_reply: 42.0,
                output_per_reply: 80.0,
                no_thinking_share: 0.0,
                median_ttft_ms,
                failure_rate: 0.0,
                baseline_median_thinking: Some(40.0),
                baseline_low: Some(30.0),
                baseline_high: Some(50.0),
                judgeable: true,
                outlier: false,
            };
            let days = vec![day("2026-11-01", Some(200.0)), day("2026-11-02", None), day("2026-11-09", Some(260.0))];
            let markers = vec![DriftMarker { date: "2026-11-06".into(), label: "Updated client".into(), kind: "client_version".into() }];
            rsx! {
                DriftBandChart { days: days.clone(), markers: markers.clone() }
                DriftBandChart { days, markers, ttft: true }
            }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert_eq!(rendered.matches("chart-drift-line").count(), 1);
        assert_eq!(rendered.matches("14d median").count(), 1);
        assert_eq!(rendered.matches("Normal band").count(), 1);
        assert_eq!(rendered.matches("Updated client").count(), 2);
        assert!(rendered.contains("Peak 260.0"));
        assert!(rendered.contains("Daily median TTFT (ms)"));
    }

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
        assert!(rendered.contains("80.0%"));
    }

    #[test]
    fn empty_activity_charts_show_no_range_data_without_hover_controls() {
        fn app() -> Element {
            rsx! {
                ActivityTrendChart { points: vec![], metric: Metric::Requests }
                CacheHitRateChart { points: vec![] }
            }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert_eq!(rendered.matches("No data for selected range").count(), 2);
        assert!(!rendered.contains("left and right arrows"));
    }

    #[test]
    fn zero_input_day_breaks_the_daily_cache_path() {
        fn app() -> Element {
            rsx! { CacheHitRateChart { points: vec![daily_point(0, 100), daily_point(1, 0), daily_point(2, 100)] } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert_eq!(rendered.matches("chart-cache-dot").count(), 2);
        assert!(!rendered.contains("activity-cache-segment"));
        assert!(rendered.contains("1970-01-02 → 1970-01-03 UTC"));
    }

    #[test]
    fn missing_day_breaks_the_daily_cache_path_and_leaves_time_proportional_space() {
        fn app() -> Element {
            rsx! { CacheHitRateChart { points: vec![daily_point(0, 100), daily_point(1, 100), daily_point(3, 100), daily_point(4, 100)] } }
        }
        let mutations = VirtualDom::new(app).rebuild_to_vec();
        let ends = svg_attribute_values(&mutations, "x2");
        let count = |x| ends.iter().filter(|&&v| v == x).count();
        assert_eq!(count("216"), 1); // Line segment end at the bucket's center.
        assert_eq!(count("328"), 0); // Missing bucket gets no line segment.
        assert_eq!(count("552"), 1);
        let rendered = format!("{mutations:?}");
        assert!(rendered.contains("1970-01-03 → 1970-01-04 UTC"));
    }

    #[test]
    fn daily_activity_sorts_buckets_and_displays_missing_days() {
        fn app() -> Element {
            let points = vec![daily_point(2, 200), daily_point(0, 100)];
            rsx! { ActivityTrendChart { points, metric: Metric::CacheHitRate } }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        let first = rendered.find("1970-01-01 → 1970-01-02 UTC").unwrap();
        let missing = rendered.find("1970-01-02 → 1970-01-03 UTC").expect("missing day should have a slot");
        let last = rendered.find("1970-01-03 → 1970-01-04 UTC").unwrap();
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
                ActivityHeatmap { cells, metric: Metric::CacheHitRate, covered_weekdays: vec![0] }
                DriftBandChart { days: drift_days, markers: vec![] }
            }
        }
        let rendered = format!("{:?}", VirtualDom::new(app).rebuild_to_vec());
        assert!(rendered.contains("chart-stage"));
        assert!(rendered.contains("Showing newest 4,000 samples"));
        assert!(rendered.contains("55.0% left"));
        assert!(!rendered.contains("Estimated projection data table"));
        assert!(rendered.contains("activity-peak"));
        assert!(rendered.contains("chart-hit"));
        assert!(rendered.contains("Cache hit rate (%) · UTC"));
        assert!(rendered.contains("Cache hit rate (%)"));
        assert!(rendered.contains("14d median"));
        assert!(rendered.contains("Normal band"));
        assert!(rendered.contains("Tue 00:00 · Outside selected window"));
        assert!(rendered.contains("Mon 00:00 · N/A · no input tokens"));
    }
}
