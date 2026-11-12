//! Pure presentation helpers (no Dioxus, no network) so they can be tested
//! without a window. The donor kept `rel_time` next to its views; pulling these
//! out keeps the UI code thin and lets `cargo test` cover the formatting rules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the unix epoch, or 0 when the clock is before it.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
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

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

/// Absolute UTC timestamp for detail rows.
pub fn fmt_dt(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Day heading for grouped intents.
pub fn fmt_day(day: NaiveDate) -> String {
    day.format("%a %d %b %Y").to_string()
}

/// Parse a `datetime-local` value as UTC (the hub stores UTC instants).
pub fn parse_local(value: &str) -> Option<DateTime<Utc>> {
    for format in ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
            return Some(naive.and_utc());
        }
    }
    None
}

/// Format a UTC instant for a `datetime-local` input (minute precision).
pub fn default_local(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M").to_string()
}

/// Subsequence match used by the command palette. Returns a higher score for
/// consecutive matches; `None` means the query is not a subsequence.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    if needle.is_empty() {
        return Some(0);
    }
    let haystack = haystack.to_lowercase();
    let mut index = 0usize;
    let mut score = 0i32;
    let mut previous: Option<usize> = None;
    for character in haystack.chars() {
        if index < needle.len() && character == needle[index] {
            score += match previous {
                Some(p) if p + 1 == index => 3,
                Some(_) => 1,
                None => 2,
            };
            previous = Some(index);
            index += 1;
            if index == needle.len() {
                return Some(score);
            }
        }
    }
    None
}

/// Indices of `haystacks` matching `needle`, best first.
pub fn fuzzy_filter(needle: &str, haystacks: &[&str]) -> Vec<usize> {
    let mut matches: Vec<(i32, usize)> =
        haystacks.iter().enumerate().filter_map(|(index, text)| fuzzy_score(needle, text).map(|score| (score, index))).collect();
    matches.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    matches.into_iter().map(|(_, index)| index).collect()
}

/// First 12 characters of a hash/id for dense rows.
pub fn short_hash(value: &str) -> String {
    value.chars().take(12).collect()
}

/// `video/*` assets preview with `<video>`; everything else with `<img>`.
pub fn is_video(mime: &str) -> bool {
    mime.trim().to_ascii_lowercase().starts_with("video/")
}

/// Download filename for an asset's data URI: `<sha12>.<ext>`.
pub fn media_name(mime: &str, sha256: &str) -> String {
    let subtype = mime.split(';').next().unwrap_or(mime).rsplit('/').next().unwrap_or("");
    let extension: String = subtype.chars().filter(char::is_ascii_alphanumeric).collect();
    let usable = !extension.is_empty() && extension.len() <= 5;
    let extension = if usable { extension.to_ascii_lowercase() } else { "bin".to_string() };
    format!("{}.{extension}", short_hash(sha256))
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
    fn datetime_helpers() {
        let dt = DateTime::parse_from_rfc3339("2026-11-09T18:30:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(fmt_dt(dt), "2026-11-09 18:30 UTC");
        assert_eq!(default_local(dt), "2026-11-09T18:30");
        assert_eq!(parse_local("2026-11-09T18:30").unwrap(), dt);
        assert_eq!(parse_local("2026-11-09T18:30:00").unwrap(), dt);
        assert!(parse_local("not a date").is_none());
        assert_eq!(fmt_day(NaiveDate::from_ymd_opt(2026, 11, 9).unwrap()), "Mon 09 Nov 2026");
    }

    #[test]
    fn fuzzy_filter_orders_and_rejects() {
        let items = ["Today", "News", "Studio", "Relationships"];
        assert_eq!(fuzzy_filter("tod", &items), vec![0]);
        assert_eq!(fuzzy_filter("ne", &items), vec![1]);
        assert!(fuzzy_filter("zzz", &items).is_empty());
        assert_eq!(fuzzy_filter("", &items), vec![0, 1, 2, 3]);
        assert!(fuzzy_score("rel", "Relationships").unwrap() > 0);
    }

    #[test]
    fn short_hash_caps() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_hash("abc"), "abc");
    }

    #[test]
    fn media_kind_and_download_name() {
        assert!(is_video("video/mp4"));
        assert!(is_video("VIDEO/WEBM"));
        assert!(!is_video("image/png"));
        assert_eq!(media_name("video/mp4", "0123456789abcdef"), "0123456789ab.mp4");
        assert_eq!(media_name("application/octet-stream", "abc"), "abc.bin");
    }
}
