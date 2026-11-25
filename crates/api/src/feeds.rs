//! Status feeds: poll each configured JSON status document and keep the
//! `monitor` notification source in step with it.
//!
//! `HUB_FEEDS_CONFIG` names a JSON object mapping feed id to its config:
//!
//! ```json
//! {"syndb": {"kind": "s3", "endpoint": "https://s3-west.nrp-nautilus.io", "region": "us-east-1",
//!            "bucket": "syndb-status", "key": "report.json",
//!            "access_key_id_file": "feed-syndb-access-key-id",
//!            "secret_access_key_file": "feed-syndb-secret-access-key",
//!            "interval_secs": 300, "stale_after_secs": 2700, "min_status": "warn"}}
//! ```
//!
//! A relative credential path is resolved against `$CREDENTIALS_DIRECTORY`.
//! Credentials are read on every poll and never logged: only the fetch error,
//! which carries no URL or header, is. systemd copies `LoadCredential` files
//! when the unit starts, so a rotated key takes effect on the next restart.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use domain::Hub;
use domain::feeds::{CheckStatus, MAX_DOC_BYTES};
use integrations::live::SecretString;
use integrations::live::s3::{S3Credentials, S3Object, get_object};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedKind {
    S3,
}

fn warn() -> CheckStatus {
    CheckStatus::Warn
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    pub kind: FeedKind,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub key: String,
    pub access_key_id_file: PathBuf,
    pub secret_access_key_file: PathBuf,
    pub interval_secs: u64,
    pub stale_after_secs: u64,
    #[serde(default = "warn")]
    pub min_status: CheckStatus,
}

/// A validated feed, ready to poll.
#[derive(Debug, Clone)]
pub struct Feed {
    pub id: String,
    pub object: S3Object,
    access_key_id_file: PathBuf,
    secret_access_key_file: PathBuf,
    pub interval: Duration,
    pub stale_after_secs: i64,
    pub min_status: CheckStatus,
}

/// Parse and check `HUB_FEEDS_CONFIG`; startup fails on any mistake.
pub fn load(path: &Path, credentials_dir: Option<&Path>) -> anyhow::Result<Vec<Feed>> {
    let body = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let configs: BTreeMap<String, FeedConfig> = serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    configs.into_iter().map(|(id, c)| validate(id, c, credentials_dir)).collect()
}

fn validate(id: String, c: FeedConfig, credentials_dir: Option<&Path>) -> anyhow::Result<Feed> {
    anyhow::ensure!(
        !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "feed id {id:?} must match [a-z0-9-]{{1,32}}"
    );
    let endpoint = reqwest::Url::parse(&c.endpoint).with_context(|| format!("feed {id}: endpoint"))?;
    anyhow::ensure!(endpoint.username().is_empty() && endpoint.password().is_none(), "feed {id}: endpoint must not carry credentials");
    anyhow::ensure!(endpoint.fragment().is_none(), "feed {id}: endpoint must have no fragment");
    let loopback = matches!(endpoint.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    anyhow::ensure!(endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback), "feed {id}: endpoint must be https");
    anyhow::ensure!(matches!(endpoint.path(), "" | "/") && endpoint.query().is_none(), "feed {id}: endpoint must have no path or query");
    for (name, value) in [("region", &c.region), ("bucket", &c.bucket), ("key", &c.key)] {
        anyhow::ensure!(!value.trim().is_empty(), "feed {id}: {name} is empty");
    }
    // A URL normalizes `.` and `..` away, so the signed path would differ from the one sent.
    anyhow::ensure!(!c.key.split('/').any(|s| s == "." || s == ".."), "feed {id}: key must not contain . or .. segments");
    // S3 bucket naming; also keeps `.` and `..` (normalized out of the URL) away.
    anyhow::ensure!(
        c.bucket.len() <= 63
            && c.bucket.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && c.bucket.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-')),
        "feed {id}: bucket must be an S3 bucket name ([a-z0-9][a-z0-9.-]*)"
    );
    anyhow::ensure!((30..=86_400).contains(&c.interval_secs), "feed {id}: interval_secs must be 30..=86400");
    anyhow::ensure!(c.stale_after_secs >= c.interval_secs, "feed {id}: stale_after_secs must be at least interval_secs");
    anyhow::ensure!(c.min_status != CheckStatus::Ok, "feed {id}: min_status must be unknown, warn or fail");
    let resolve = |file: PathBuf| -> anyhow::Result<PathBuf> {
        if file.is_absolute() {
            return Ok(file);
        }
        let dir =
            credentials_dir.with_context(|| format!("feed {id}: {} is relative and CREDENTIALS_DIRECTORY is unset", file.display()))?;
        Ok(dir.join(file))
    };
    Ok(Feed {
        object: S3Object { endpoint, region: c.region, bucket: c.bucket, key: c.key },
        access_key_id_file: resolve(c.access_key_id_file)?,
        secret_access_key_file: resolve(c.secret_access_key_file)?,
        interval: Duration::from_secs(c.interval_secs),
        stale_after_secs: c.stale_after_secs.min(i64::MAX as u64) as i64,
        min_status: c.min_status,
        id,
    })
}

fn read_secret(path: &Path, what: &str) -> Result<String, String> {
    // The io error names the cause, never the content.
    let value = std::fs::read_to_string(path).map_err(|e| format!("reading {what}: {}", e.kind()))?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{what} is empty"));
    }
    Ok(value.to_string())
}

/// Fetch and evaluate one feed. Every string in the result is safe to log.
pub async fn poll(http: &reqwest::Client, feed: &Feed) -> domain::feeds::Evaluation {
    let fetched = async {
        let creds = S3Credentials {
            access_key_id: SecretString::new(read_secret(&feed.access_key_id_file, "access_key_id_file")?),
            secret_access_key: SecretString::new(read_secret(&feed.secret_access_key_file, "secret_access_key_file")?),
        };
        let body = get_object(http, &feed.object, &creds, MAX_DOC_BYTES).await?;
        domain::feeds::parse(&body)
    }
    .await;
    domain::feeds::evaluate(&feed.id, "s3", feed.min_status, feed.stale_after_secs, fetched, chrono::Utc::now())
}

/// The producer loop. Every feed is due on the first pass, so the first sync
/// already sees all of them; a feed that cannot be read keeps its check rows.
/// With no feeds it syncs once, resolving rows of feeds removed from config.
// ponytail: feeds poll serially (30 s timeout each) and sync once per pass;
// poll concurrently and sync per feed prefix if one hub ever carries many feeds.
pub async fn run(hub: Hub, feeds: Vec<Feed>) {
    let http = match integrations::live::s3::client() {
        Ok(http) => http,
        Err(error) => {
            hub.record_sync_error("monitor", &error);
            return tracing::error!(%error, "feeds: no HTTP client");
        }
    };
    let mut due = vec![tokio::time::Instant::now(); feeds.len()];
    let mut last: BTreeMap<String, domain::feeds::Evaluation> = BTreeMap::new();
    loop {
        for (feed, due) in feeds.iter().zip(due.iter_mut()) {
            if *due > tokio::time::Instant::now() {
                continue;
            }
            let eval = poll(&http, feed).await;
            if let Some(problem) = &eval.state.problem {
                tracing::warn!(feed = %feed.id, state = %eval.state.state, %problem, "feeds: poll");
            }
            if let Ok(mut states) = hub.feeds.write()
                && let Some(slot) = states.iter_mut().find(|s| s.id == feed.id)
            {
                *slot = eval.state.clone();
            }
            last.insert(feed.id.clone(), eval);
            *due = tokio::time::Instant::now() + feed.interval;
        }
        let synced = sync(&hub, &last).await;
        match due.iter().min() {
            Some(next) => tokio::time::sleep_until(*next).await,
            // No feeds: one successful sync resolves removed feeds' rows.
            None if synced.is_ok() => return,
            None => tokio::time::sleep(Duration::from_secs(60)).await,
        }
    }
}

/// Sync the `monitor` source from each feed's latest poll. A feed that is not
/// fresh contributes only its feed row and keeps its check rows untouched;
/// a fresh one resolves the checks it no longer reports.
pub async fn sync(hub: &Hub, last: &BTreeMap<String, domain::feeds::Evaluation>) -> domain::DResult<usize> {
    let keep: Vec<String> = last.iter().filter(|(_, e)| !e.fresh).map(|(id, _)| format!("monitor:{id}:")).collect();
    let current = last.values().flat_map(|e| e.raises.clone()).collect();
    let synced = domain::notifications::sync_keeping(hub, "monitor", current, &keep).await;
    match &synced {
        Ok(_) => hub.record_sync_ok("monitor"),
        Err(error) => {
            hub.record_sync_error("monitor", error);
            tracing::error!(%error, "feeds: notification sync failed");
        }
    }
    synced
}

/// `pending` rows for `/v1/status` until the first poll lands.
pub fn pending_states(feeds: &[Feed]) -> Vec<contracts::FeedState> {
    feeds
        .iter()
        .map(|f| contracts::FeedState {
            id: f.id.clone(),
            kind: "s3".into(),
            state: "pending".into(),
            checked_at: None,
            generated_at: None,
            raised: 0,
            problem: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};

    const ACCESS_KEY: &str = "AKIDFEEDTEST";
    const SECRET_KEY: &str = "s3cr3t-never-in-logs-0123456789";

    /// A fake S3: answers the report for a signed request from `ACCESS_KEY`,
    /// and an S3 error document otherwise.
    async fn fake_s3(report: &'static str) -> String {
        let app = axum::Router::new().route(
            "/syndb-status/report.json",
            axum::routing::get(move |headers: HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
                let signed = auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/"))
                    && auth.contains("/us-east-1/s3/aws4_request")
                    && headers.contains_key("x-amz-date");
                if signed {
                    (StatusCode::OK, report.to_string())
                } else {
                    (StatusCode::FORBIDDEN, "<Error><Code>InvalidAccessKeyId</Code></Error>".to_string())
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn feed(endpoint: &str, dir: &Path) -> Feed {
        let config = serde_json::json!({"syndb": {
            "kind": "s3", "endpoint": endpoint, "region": "us-east-1", "bucket": "syndb-status", "key": "report.json",
            "access_key_id_file": "akid", "secret_access_key_file": "secret", "interval_secs": 300, "stale_after_secs": 2700}});
        let path = dir.join("feeds.json");
        std::fs::write(&path, config.to_string()).unwrap();
        load(&path, Some(dir)).unwrap().remove(0)
    }

    #[tokio::test]
    async fn polls_a_signed_object_and_never_leaks_the_secret() {
        let report = Box::leak(
            format!(
                r#"{{"metadata":{{"timestamp":"{}"}},"summary":{{}},"checks":[{{"id":"backup.clickhouse","status":"fail","message":"no backup"}}]}}"#,
                chrono::Utc::now().to_rfc3339()
            )
            .into_boxed_str(),
        );
        let endpoint = fake_s3(report).await;
        let dir = std::env::temp_dir().join(format!("hub-feed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("akid"), format!("{ACCESS_KEY}\n")).unwrap();
        std::fs::write(dir.join("secret"), SECRET_KEY).unwrap();
        let feed = feed(&endpoint, &dir);
        let http = integrations::live::s3::client().unwrap();

        let ok = poll(&http, &feed).await;
        assert_eq!(ok.state.state, "ok", "{:?}", ok.state.problem);
        assert_eq!(ok.raises[0].dedupe_key, "monitor:syndb:backup.clickhouse");

        // A wrong key: the error names the S3 code only.
        std::fs::write(dir.join("akid"), SECRET_KEY).unwrap();
        let denied = poll(&http, &feed).await;
        assert_eq!(denied.state.problem.as_deref(), Some("HTTP 403 InvalidAccessKeyId"));
        // A missing file: the error names the file kind, not its path content.
        std::fs::remove_file(dir.join("secret")).unwrap();
        let missing = poll(&http, &feed).await;
        assert_eq!(missing.state.problem.as_deref(), Some("reading secret_access_key_file: entity not found"));

        let mut seen = format!("{feed:?}");
        for eval in [&ok, &denied, &missing] {
            seen += &format!("{:?}{:?}", eval.state, eval.raises);
        }
        assert!(!seen.contains(SECRET_KEY) && !seen.contains("Signature="), "{seen}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn config_is_checked_at_startup() {
        let base = serde_json::json!({"kind": "s3", "endpoint": "https://s3-west.nrp-nautilus.io", "region": "us-east-1",
            "bucket": "b", "key": "k", "access_key_id_file": "/run/a", "secret_access_key_file": "/run/b",
            "interval_secs": 300, "stale_after_secs": 2700});
        let check = |id: &str, patch: serde_json::Value| {
            let mut c = base.clone();
            c.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
            serde_json::from_value::<FeedConfig>(c).map_err(anyhow::Error::from).and_then(|c| validate(id.into(), c, None))
        };
        let feed = check("syndb", serde_json::json!({})).unwrap();
        assert_eq!(feed.min_status, CheckStatus::Warn);
        for (id, patch) in [
            ("Bad_Id", serde_json::json!({})),
            ("syndb", serde_json::json!({"endpoint": "http://s3.example.com"})),
            ("syndb", serde_json::json!({"stale_after_secs": 60})),
            ("syndb", serde_json::json!({"min_status": "ok"})),
            ("syndb", serde_json::json!({"secret_access_key": "inline"})),
            ("syndb", serde_json::json!({"access_key_id_file": "relative"})),
            ("syndb", serde_json::json!({"endpoint": "https://user:pw@s3-west.nrp-nautilus.io"})),
            ("syndb", serde_json::json!({"key": "a/../report.json"})),
            ("syndb", serde_json::json!({"bucket": ".."})),
            ("syndb", serde_json::json!({"endpoint": "https://s3-west.nrp-nautilus.io/?X-Amz-Signature=abc"})),
            ("syndb", serde_json::json!({"interval_secs": u64::MAX, "stale_after_secs": u64::MAX})),
        ] {
            assert!(check(id, patch.clone()).is_err(), "{id} {patch}");
        }
    }
}
