//! Pure presentation helpers (no Dioxus, no network) so they can be tested
//! without a window. The donor kept `rel_time` next to its views; pulling these
//! out keeps the UI code thin and lets `cargo test` cover the formatting rules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the unix epoch, or 0 when the clock is before it.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Relative time from a unix-ms timestamp (donor `rel_time`). Timestamps in the
/// future or at the epoch read as "now".
pub fn rel_time_at(now_ms: u64, then_ms: u64) -> String {
    if then_ms == 0 || then_ms >= now_ms {
        return "now".to_string();
    }
    let secs = (now_ms - then_ms) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Relative time using the wall clock.
pub fn rel_time(then_ms: u64) -> String {
    rel_time_at(now_ms(), then_ms)
}

/// Collapse whitespace and cap a preview at `max` characters (plus an ellipsis).
pub fn preview(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// A compact id for dense rows: first 8 hex characters.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_time_boundaries() {
        let now = 1_000_000_000_000;
        assert_eq!(rel_time_at(now, 0), "now");
        assert_eq!(rel_time_at(now, now + 5_000), "now");
        assert_eq!(rel_time_at(now, now - 5_000), "5s ago");
        assert_eq!(rel_time_at(now, now - 59_000), "59s ago");
        assert_eq!(rel_time_at(now, now - 60_000), "1m ago");
        assert_eq!(rel_time_at(now, now - 3_599_000), "59m ago");
        assert_eq!(rel_time_at(now, now - 3_600_000), "1h ago");
        assert_eq!(rel_time_at(now, now - 86_399_000), "23h ago");
        assert_eq!(rel_time_at(now, now - 86_400_000), "1d ago");
    }

    #[test]
    fn preview_flattens_and_caps() {
        assert_eq!(preview("a  b\n c", 20), "a b c");
        assert_eq!(preview("abcdef", 3), "abc…");
        assert_eq!(preview("abc", 3), "abc");
    }

    #[test]
    fn short_id_truncates() {
        assert_eq!(short_id("0123456789"), "01234567");
        assert_eq!(short_id("abc"), "abc");
    }
}
