//! Status feeds: a JSON status document a project publishes and the hub polls.
//!
//! Contract v1 is `{metadata: {timestamp}, summary, checks: [{id, status,
//! message, remediation, ...}]}`; unknown fields are ignored. The document is
//! untrusted: it is size-capped by the fetcher, every string is length-capped
//! and stripped of control characters here, and it only ever becomes plain
//! notification text.

use chrono::{DateTime, Utc};
use contracts::{FeedState, Severity};
use serde::Deserialize;

use crate::notifications::Raise;

/// Largest document the fetcher reads.
pub const MAX_DOC_BYTES: usize = 1 << 20;
const MAX_CHECKS: usize = 1000;
/// A check id is stored verbatim in `monitor:<feed>:<id>`, so it must already
/// be in the key alphabet: 128 bytes minus `monitor:`, a 32-byte feed id and `:`.
const MAX_ID: usize = 80;
const MAX_TEXT: usize = 1000;
/// Clock difference tolerated between the producer and the hub.
const CLOCK_SKEW_SECS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Unknown,
    Warn,
    Fail,
}

impl CheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unknown => "unknown",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Check {
    pub id: String,
    pub status: CheckStatus,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub remediation: Option<String>,
}

#[derive(Debug)]
pub struct Report {
    pub generated_at: DateTime<Utc>,
    pub checks: Vec<Check>,
}

/// Plain one-line text: control characters become spaces, at most `max` chars.
fn clean(text: &str, max: usize) -> String {
    let text = text.trim();
    let mut out: String = text.chars().take(max).map(|c| if c.is_control() { ' ' } else { c }).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

/// Parse and validate one document. The error is safe to show and log.
pub fn parse(bytes: &[u8]) -> Result<Report, String> {
    #[derive(Deserialize)]
    struct Doc {
        metadata: Metadata,
        checks: Vec<Check>,
    }
    #[derive(Deserialize)]
    struct Metadata {
        timestamp: String,
    }
    // Errors name a category and a place, never document text: it reaches
    // logs, `/v1/status` and notifications, and the document is untrusted.
    let doc: Doc = serde_json::from_slice(bytes)
        .map_err(|e| format!("invalid status document ({:?} error at line {} column {})", e.classify(), e.line(), e.column()))?;
    let generated_at = DateTime::parse_from_rfc3339(&doc.metadata.timestamp)
        .map_err(|_| "metadata.timestamp is not RFC 3339".to_string())?
        .with_timezone(&Utc);
    if doc.checks.len() > MAX_CHECKS {
        return Err(format!("{} checks exceed the limit of {MAX_CHECKS}", doc.checks.len()));
    }
    let mut checks = doc.checks;
    let mut seen = std::collections::BTreeSet::new();
    for (index, check) in checks.iter_mut().enumerate() {
        // Ids are never folded: two ids folding into one key would merge (and
        // could hide) distinct checks, and `feed` is the feed's own row.
        let valid = check.id.len() <= MAX_ID
            && check.id.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && check.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
        if !valid || check.id == "feed" {
            return Err(format!("checks[{index}].id must match [a-z0-9][a-z0-9._-]{{0,{}}} and not be \"feed\"", MAX_ID - 1));
        }
        if !seen.insert(check.id.clone()) {
            return Err(format!("checks[{index}].id repeats an earlier check"));
        }
        check.message = check.message.as_deref().map(|m| clean(m, MAX_TEXT));
        check.remediation = check.remediation.as_deref().map(|m| clean(m, MAX_TEXT));
    }
    Ok(Report { generated_at, checks })
}

/// What one poll means for notifications and for `/v1/status`.
pub struct Evaluation {
    pub state: FeedState,
    /// The feed's raises. When `fresh` is false this is only
    /// `monitor:<feed>:feed`, and the feed's check rows must be left as they are.
    pub raises: Vec<Raise>,
    pub fresh: bool,
}

/// Turn one poll into raises. A fresh report raises one row per check at or
/// above `min_status`; checks that recovered are simply absent, so the sync
/// resolves them. A stale report or a failed fetch raises only the feed row.
pub fn evaluate(
    feed: &str,
    kind: &str,
    min_status: CheckStatus,
    stale_after_secs: i64,
    polled: Result<Report, String>,
    now: DateTime<Utc>,
) -> Evaluation {
    let mut state = FeedState {
        id: feed.into(),
        kind: kind.into(),
        state: "ok".into(),
        checked_at: Some(now),
        generated_at: None,
        raised: 0,
        problem: None,
    };
    let problem = match polled {
        Err(error) => {
            state.state = "error".into();
            error
        }
        Ok(report) => {
            state.generated_at = Some(report.generated_at);
            let mut raises = Vec::new();
            for check in report.checks.iter().filter(|c| c.status >= min_status && c.status != CheckStatus::Ok) {
                let severity = match check.status {
                    CheckStatus::Fail => Severity::Critical,
                    CheckStatus::Warn => Severity::Warning,
                    _ => Severity::Info,
                };
                let mut body = check.message.clone().unwrap_or_default();
                if let Some(fix) = check.remediation.as_deref().filter(|r| !r.is_empty()) {
                    body = format!("{body}\nRemediation: {fix}");
                }
                raises.push(Raise {
                    source: "monitor".into(),
                    dedupe_key: format!("monitor:{feed}:{}", check.id),
                    severity,
                    title: format!("{feed}: {} {}", check.id, check.status.as_str()),
                    body,
                    subject_type: None,
                    subject_id: None,
                    url: None,
                });
            }
            state.raised = raises.len() as u32;
            let age = now.signed_duration_since(report.generated_at).num_seconds();
            if age <= stale_after_secs && age >= -CLOCK_SKEW_SECS {
                return Evaluation { state, raises, fresh: true };
            }
            state.state = "stale".into();
            // Names the timestamp, not the age, so an acknowledged stale feed
            // stays acknowledged until a new document arrives.
            format!("metadata.timestamp {} is outside the {stale_after_secs} s freshness window", report.generated_at.to_rfc3339())
        }
    };
    state.problem = Some(problem.clone());
    let title = if state.state == "stale" { format!("Status feed {feed} is stale") } else { format!("Status feed {feed} is unreadable") };
    let raise = Raise {
        source: "monitor".into(),
        dedupe_key: format!("monitor:{feed}:feed"),
        severity: Severity::Warning,
        title,
        body: problem,
        subject_type: None,
        subject_id: None,
        url: None,
    };
    Evaluation { state, raises: vec![raise], fresh: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn doc(timestamp: &str) -> String {
        format!(
            r#"{{"metadata":{{"timestamp":"{timestamp}","namespace":"syndb","generator":"syndb 0.11.2"}},
               "summary":{{"total":4,"ok":1,"warn":1,"fail":1,"unknown":1,"actionable_failures":1}},
               "checks":[
                 {{"id":"backup.clickhouse","status":"fail","severity":"high","scope":"namespace","message":"no backup\nin 30 h <b>","remediation":"run the backup job","actionable":true,"extra":1}},
                 {{"id":"pvc.usage","status":"warn","message":"85% full","remediation":""}},
                 {{"id":"cert.expiry","status":"unknown","message":"no data"}},
                 {{"id":"pods.ready","status":"ok","message":"all ready"}}
               ]}}"#
        )
    }

    fn run(body: &str, min: CheckStatus) -> Evaluation {
        evaluate("syndb", "s3", min, 2700, parse(body.as_bytes()), now())
    }

    #[test]
    fn fail_is_critical_and_min_status_filters() {
        let eval = run(&doc("2026-11-18T11:55:00Z"), CheckStatus::Warn);
        assert!(eval.fresh);
        assert_eq!(eval.state.state, "ok");
        let found: Vec<_> = eval.raises.iter().map(|r| (r.dedupe_key.as_str(), r.severity)).collect();
        assert_eq!(found, [("monitor:syndb:backup.clickhouse", Severity::Critical), ("monitor:syndb:pvc.usage", Severity::Warning)]);
        let fail = &eval.raises[0];
        assert_eq!(fail.title, "syndb: backup.clickhouse fail");
        // One line of plain text: the newline is gone, markup is left as inert text.
        assert_eq!(fail.body, "no backup in 30 h <b>\nRemediation: run the backup job");
        assert_eq!(eval.raises[1].body, "85% full", "an empty remediation adds nothing");

        let all = run(&doc("2026-11-18T11:55:00Z"), CheckStatus::Unknown);
        assert_eq!(all.raises.iter().find(|r| r.dedupe_key == "monitor:syndb:cert.expiry").unwrap().severity, Severity::Info);
        assert!(!all.raises.iter().any(|r| r.dedupe_key.ends_with("pods.ready")), "ok never notifies");
    }

    #[test]
    fn stale_feed_raises_only_the_feed_row() {
        let eval = run(&doc("2026-11-18T11:00:00Z"), CheckStatus::Warn);
        assert!(!eval.fresh);
        assert_eq!(eval.state.state, "stale");
        assert_eq!(eval.raises.len(), 1);
        assert_eq!(eval.raises[0].dedupe_key, "monitor:syndb:feed");
        assert_eq!(eval.raises[0].severity, Severity::Warning);
        // Far-future timestamps are not "fresh forever".
        assert!(!run(&doc("2026-11-18T13:00:00Z"), CheckStatus::Warn).fresh);
    }

    #[test]
    fn parse_errors_raise_only_the_feed_row() {
        for bad in [
            "not json".to_string(),
            doc("yesterday"),
            doc("2026-11-18T11:55:00Z").replace(r#""status":"warn""#, r#""status":"bogus""#),
            doc("2026-11-18T11:55:00Z").replace(r#""id":"pvc.usage""#, r#""id":"  ""#),
            r#"{"metadata":{"timestamp":"2026-11-18T11:55:00Z"}}"#.to_string(),
        ] {
            let eval = run(&bad, CheckStatus::Warn);
            assert!(!eval.fresh, "{bad}");
            assert_eq!(eval.state.state, "error");
            assert_eq!(eval.raises.iter().map(|r| r.dedupe_key.as_str()).collect::<Vec<_>>(), ["monitor:syndb:feed"]);
            assert!(eval.state.problem.as_ref().unwrap().chars().count() <= MAX_TEXT + 1);
        }
    }

    #[test]
    fn ids_that_would_fold_or_collide_are_parse_errors() {
        let ok = doc("2026-11-18T11:55:00Z");
        for id in ["Disk", "feed", "a/b", "x:y", "", &"x".repeat(MAX_ID + 1)] {
            let eval = run(&ok.replace("pvc.usage", id), CheckStatus::Warn);
            assert_eq!(eval.state.state, "error", "{id:?}");
        }
        let twice = run(&ok.replace("pvc.usage", "backup.clickhouse"), CheckStatus::Warn);
        assert_eq!(twice.state.problem.as_deref(), Some("checks[1].id repeats an earlier check"));
        assert!(run(&ok.replace("pvc.usage", &"x".repeat(MAX_ID)), CheckStatus::Warn).fresh);
    }
}
