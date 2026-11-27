use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use chrono::{DateTime, TimeDelta, Utc};
use contracts::providers::{
    AccountErrorKind, AccountErrorSource, QuotaObservationState, QuotaStatus, QuotaUnit, QuotaWindow, QuotaWindowKind, ResetCadence,
    RouteStrategy,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex};
use uuid::Uuid;

use crate::{Proxy, clients::register_t3code, config::Config, error_classify::local_error, management, pool::Cooldown, router};

#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    pub name: &'static str,
    pub passed: bool,
    pub duration_ms: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub passed: bool,
    pub passed_count: usize,
    pub total_count: usize,
    pub duration_ms: u64,
    pub checks: Vec<CheckOutcome>,
}

impl VerifyReport {
    pub fn format_human(&self) -> String {
        let mut out = String::new();
        for check in &self.checks {
            let tag = if check.passed { "PASS" } else { "FAIL" };
            out.push_str(&format!("[{tag}] {} ({}ms) — {}\n", check.name, check.duration_ms, check.detail));
        }
        out.push_str(&format!("\nSummary: {}/{} checks passed in {}ms\n", self.passed_count, self.total_count, self.duration_ms));
        out
    }
}

/// Runs the complete offline algorithmic T3 Code + Antigravity proxy verification suite.
///
/// Every check runs against ephemeral loopback HTTP servers and isolated temp directories
/// with zero impact on live proxy state, production credentials, or external networks.
pub async fn run_all() -> VerifyReport {
    let suite_start = Instant::now();
    let checks = vec![
        run_check("t3code_settings_registration", check_t3code_settings_registration()).await,
        run_check("t3code_management_api_contract", check_t3code_management_api_contract()).await,
        run_check("antigravity_cloudcode_endpoints", check_antigravity_cloudcode_endpoints()).await,
        run_check("antigravity_mcp_tooling_and_streaming", check_antigravity_mcp_tooling_and_streaming()).await,
        run_check("antigravity_cache_hit_preservation", check_antigravity_cache_hit_preservation()).await,
        run_check("antigravity_quota_exhaustion_and_thread_auto_resume", check_antigravity_quota_exhaustion_and_thread_auto_resume()).await,
        run_check("antigravity_in_band_errors", check_antigravity_in_band_errors()).await,
        run_check("antigravity_dynamic_auth_and_401_recovery", check_antigravity_dynamic_auth_and_401_recovery()).await,
        run_check("hub_activity_cache_hit_sync", check_hub_activity_cache_hit_sync()).await,
    ];
    let passed_count = checks.iter().filter(|c| c.passed).count();
    let total_count = checks.len();
    VerifyReport {
        passed: passed_count == total_count,
        passed_count,
        total_count,
        duration_ms: suite_start.elapsed().as_millis() as u64,
        checks,
    }
}

async fn run_check<F>(name: &'static str, fut: F) -> CheckOutcome
where
    F: std::future::Future<Output = Result<String>>,
{
    let started = Instant::now();
    match fut.await {
        Ok(detail) => CheckOutcome { name, passed: true, duration_ms: started.elapsed().as_millis() as u64, detail },
        Err(err) => CheckOutcome { name, passed: false, duration_ms: started.elapsed().as_millis() as u64, detail: format!("{err:#}") },
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("fleetix-proxy-verify-{}", Uuid::new_v4()));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct EphemeralServer {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl EphemeralServer {
    async fn start(app: Router) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await;
        });
        Ok(Self { addr, task })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn base_url(&self) -> Result<reqwest::Url> {
        Ok(reqwest::Url::parse(&format!("http://{}", self.addr))?)
    }
}

impl Drop for EphemeralServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(10)).build()?)
}

fn make_proxy(dir: &TempDir, mock_url: &reqwest::Url, auth_files: &[(&str, Value)]) -> Result<Arc<Proxy>> {
    let state_dir = dir.path().join("state");
    let auth_dir = dir.path().join("auth");
    let t3_home = dir.path().join("t3");
    let client_key = dir.path().join("client.key");
    let mgmt_key = dir.path().join("mgmt.key");
    let usage_key = dir.path().join("usage.key");
    fs::create_dir_all(&auth_dir)?;
    fs::write(&client_key, "verify-client-key\n")?;
    fs::write(&mgmt_key, "verify-mgmt-key\n")?;
    fs::write(&usage_key, "verify-usage-key\n")?;
    for (name, value) in auth_files {
        fs::write(auth_dir.join(name), serde_json::to_vec(value)?)?;
    }
    let mut cfg =
        Config::new("127.0.0.1:0".parse()?, &state_dir, Some(auth_dir), false, Some(client_key), Some(mgmt_key), Some(usage_key))?;
    cfg.home = dir.path().to_path_buf();
    cfg.t3code_home = t3_home;
    cfg.anthropic = mock_url.clone();
    cfg.openai = mock_url.clone();
    cfg.codex = mock_url.clone();
    cfg.antigravity = mock_url.clone();
    let mut proxy = Proxy::new(cfg)?;
    proxy.set_mock_endpoints(mock_url.clone());
    Ok(Arc::new(proxy))
}

fn google_cred(access: &str, refresh: &str, email: &str, project: &str) -> Value {
    json!({
        "type": "antigravity",
        "email": email,
        "access_token": access,
        "refresh_token": refresh,
        "client_id": "verify-google-client.apps.googleusercontent.com",
        "client_secret": "verify-google-secret",
        "project_id": project,
        "expires_at": (Utc::now() + TimeDelta::hours(2)).to_rfc3339(),
    })
}

fn claude_cred(access: &str, email: &str) -> Value {
    json!({
        "type": "claude",
        "email": email,
        "access_token": access,
        "expires_at": (Utc::now() + TimeDelta::hours(2)).to_rfc3339(),
    })
}

fn codex_cred(access: &str, email: &str, account_id: &str) -> Value {
    json!({
        "type": "codex",
        "email": email,
        "access_token": access,
        "account_id": account_id,
        "plan_type": "pro",
        "expires_at": (Utc::now() + TimeDelta::hours(2)).to_rfc3339(),
    })
}

/// Renders a Google `google.rpc.Status` JSON error payload the exact way Go's `genai.APIError.Error()`
/// renders it in `agy` / `antigravity-acp`, matching the input parsed by T3 Code's `AntigravityTurnOutcome.ts`.
fn render_go_genai_api_error(body: &Value) -> Result<String> {
    let err = body.get("error").and_then(Value::as_object).ok_or_else(|| anyhow!("missing top-level error object"))?;
    let code = err.get("code").and_then(Value::as_i64).ok_or_else(|| anyhow!("missing numeric error.code"))?;
    let message = err.get("message").and_then(Value::as_str).ok_or_else(|| anyhow!("missing string error.message"))?;
    let status = err.get("status").and_then(Value::as_str).ok_or_else(|| anyhow!("missing string error.status"))?;
    let details = err.get("details").and_then(Value::as_array).cloned().unwrap_or_default();
    let rendered_details: Vec<String> = details.iter().map(render_go_value).collect();
    Ok(format!("Error {code}, Message: {message}, Status: {status}, Details: [{}]", rendered_details.join(" ")))
}

fn message_less_rate_limit() -> Value {
    json!({"error":{"code":8,"details":[{"@type":"google.rpc.ErrorInfo","reason":"RATE_LIMITED"}]}})
}

#[cfg(test)]
#[test]
fn google_message_less_429_with_non_quota_reason_is_a_t3_usage_limit() {
    let now = Utc::now();
    for message in [None, Some(""), Some(" \t")] {
        let mut body = message_less_rate_limit();
        if let Some(message) = message {
            body["error"]["message"] = json!(message);
        }
        router::normalize_google_quota(&mut body, "gemini", now);
        assert_eq!(body["error"]["code"], 429);
        assert_eq!(body["error"]["status"], "RESOURCE_EXHAUSTED");
        assert_eq!(body["error"]["message"], "Quota exhausted for model gemini.");
        assert_eq!(body["error"]["details"][0]["reason"], "RATE_LIMITED");
        let rendered = render_go_genai_api_error(&body).unwrap();
        let outcome = parse_t3code_antigravity_outcome(&rendered, now);
        assert_eq!(outcome.class, "usage_limit");
        assert_eq!(outcome.code, "RESOURCE_EXHAUSTED");
        assert!(outcome.retryable);
        assert!(outcome.reset_at.is_none());
    }
}

fn render_go_value(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut sorted: BTreeMap<&str, &Value> = BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.as_str(), v);
            }
            let inner: Vec<String> = sorted.into_iter().map(|(k, v)| format!("{k}:{}", render_go_value(v))).collect();
            format!("map[{}]", inner.join(" "))
        }
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(render_go_value).collect();
            format!("[{}]", inner.join(" "))
        }
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "<nil>".into(),
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct ParsedT3Outcome {
    class: &'static str,
    code: String,
    retryable: bool,
    reset_at: Option<DateTime<Utc>>,
}

/// Mirrors T3 Code's `apps/server/src/provider/acp/AntigravityTurnOutcome.ts` (`turnFailure` + `resetAtFrom`)
/// to algorithmically verify that rendered proxy errors trigger `UsageLimitRecoveryWorker` auto-resumption.
#[cfg(test)]
fn parse_t3code_antigravity_outcome(text: &str, now: DateTime<Utc>) -> ParsedT3Outcome {
    let api_error = parse_go_api_error_header(text);
    let quota_exhausted = contains_word(text, "reason:QUOTA_EXHAUSTED");
    let reset_at = extract_t3_reset_at(text, now);
    if quota_exhausted {
        return ParsedT3Outcome {
            class: "usage_limit",
            code: api_error.map(|(_, _, status)| status).unwrap_or_else(|| "QUOTA_EXHAUSTED".into()),
            retryable: true,
            reset_at,
        };
    }
    let Some((http_status, _message, status)) = api_error else {
        return ParsedT3Outcome { class: "provider_error", code: "UNKNOWN".into(), retryable: false, reset_at: None };
    };
    let rate_limited = http_status == 429 || status == "RESOURCE_EXHAUSTED";
    ParsedT3Outcome {
        class: if rate_limited { "usage_limit" } else { "provider_error" },
        code: status,
        retryable: rate_limited || http_status >= 500,
        reset_at,
    }
}

#[cfg(test)]
fn parse_go_api_error_header(text: &str) -> Option<(u16, String, String)> {
    let rest = text.strip_prefix("Error ").or_else(|| text.split_once("Error ").map(|(_, tail)| tail))?;
    let (code_str, after_code) = rest.split_once(", Message: ")?;
    if code_str.len() != 3 || !code_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let code: u16 = code_str.parse().ok()?;
    let (message, after_msg) = after_code.split_once(", Status: ")?;
    if message.is_empty() {
        return None;
    }
    let status_len = after_msg.bytes().take_while(|b| b.is_ascii_uppercase() || *b == b'_').count();
    if status_len == 0 {
        return None;
    }
    let status = after_msg[..status_len].to_owned();
    Some((code, message.to_owned(), status))
}

#[cfg(test)]
fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(haystack.as_bytes()[abs - 1]);
        let end = abs + needle.len();
        let after_ok = end == haystack.len() || !is_word_byte(haystack.as_bytes()[end]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

#[cfg(test)]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
fn extract_token_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let mut start = 0;
    while let Some(pos) = text[start..].find(key) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(text.as_bytes()[abs - 1]);
        if before_ok {
            let rest = &text[abs + key.len()..];
            let end = rest.find(|c: char| c.is_whitespace() || c == ']').unwrap_or(rest.len());
            if end > 0 {
                return Some(&rest[..end]);
            }
        }
        start = abs + 1;
    }
    None
}

#[cfg(test)]
fn extract_t3_reset_at(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(ts) = extract_token_value(text, "quotaResetTimeStamp:")
        && let Ok(parsed) = ts.parse::<DateTime<Utc>>()
    {
        return Some(parsed);
    }
    let delay = extract_token_value(text, "quotaResetDelay:").or_else(|| extract_token_value(text, "retryDelay:"))?;
    let ms = parse_go_duration_ms(delay)?;
    (ms > 0).then(|| now + TimeDelta::milliseconds(ms))
}

#[cfg(test)]
fn parse_go_duration_ms(mut raw: &str) -> Option<i64> {
    if raw.is_empty() {
        return None;
    }
    let mut total_ms = 0.0_f64;
    while !raw.is_empty() {
        let split = raw.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (num, unit_tail) = raw.split_at(split);
        let value: f64 = num.parse().ok()?;
        let (factor, rest) = if let Some(r) = unit_tail.strip_prefix("ns") {
            (1e-6, r)
        } else if let Some(r) =
            unit_tail.strip_prefix("us").or_else(|| unit_tail.strip_prefix("µs")).or_else(|| unit_tail.strip_prefix("μs"))
        {
            (1e-3, r)
        } else if let Some(r) = unit_tail.strip_prefix("ms") {
            (1.0, r)
        } else if let Some(r) = unit_tail.strip_prefix('s') {
            (1_000.0, r)
        } else if let Some(r) = unit_tail.strip_prefix('m') {
            (60_000.0, r)
        } else {
            (3_600_000.0, unit_tail.strip_prefix('h')?)
        };
        total_ms += value * factor;
        raw = rest;
    }
    Some(total_ms.round() as i64)
}

async fn check_t3code_settings_registration() -> Result<String> {
    let dir = TempDir::new()?;
    let t3_home = dir.path().join("t3code-home");
    let userdata_dir = t3_home.join("userdata");
    fs::create_dir_all(&userdata_dir)?;
    let settings_path = userdata_dir.join("settings.json");
    fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&json!({
            "theme": "dark",
            "usageLimitSources": {
                "other-source": {"label": "Other", "kind": "cliproxy", "url": "http://127.0.0.1:9999", "managementKey": "keep-me", "enabled": true}
            }
        }))?,
    )?;
    register_t3code(&t3_home, "fleetix-proxy", "Fleetix Agent Proxy", "http://127.0.0.1:8317", "usage-secret-123")?;
    let parsed: Value = serde_json::from_slice(&fs::read(&settings_path)?)?;
    ensure!(parsed["theme"] == "dark", "existing settings key was lost");
    let sources = parsed["usageLimitSources"].as_object().ok_or_else(|| anyhow!("missing usageLimitSources object"))?;
    ensure!(sources.len() == 2, "expected 2 sources, got {}", sources.len());
    ensure!(sources.contains_key("other-source"), "existing source entry was lost");
    let ours = sources.get("fleetix-proxy").ok_or_else(|| anyhow!("fleetix-proxy entry missing"))?;
    ensure!(ours["kind"] == "cliproxy", "kind must be cliproxy");
    ensure!(ours["url"] == "http://127.0.0.1:8317", "url mismatch");
    ensure!(ours["managementKey"] == "usage-secret-123", "managementKey must contain the usage key");
    ensure!(ours["enabled"] == true, "enabled must be true");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&settings_path)?.permissions().mode() & 0o777;
        ensure!(mode == 0o600, "settings.json permissions expected 0600, got {:o}", mode);
    }
    Ok("userdata/settings.json atomically updated with cliproxy source, existing keys preserved, 0600 mode".into())
}

async fn check_t3code_management_api_contract() -> Result<String> {
    let dir = TempDir::new()?;
    let consumed = Arc::new(AtomicUsize::new(0));
    let mock = EphemeralServer::start(
        Router::new()
            .route(
                "/api/oauth/usage",
                get(|headers: HeaderMap| async move {
                    let auth = router::bearer(&headers).unwrap_or_default().to_owned();
                    Json(json!({
                        "five_hour": {"utilization": 12.0, "resets_at": (Utc::now() + TimeDelta::hours(4)).to_rfc3339()},
                        "seen_token": auth,
                    }))
                }),
            )
            .route(
                "/backend-api/wham/usage",
                get(|headers: HeaderMap| async move {
                    let acct = headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                    Json(json!({
                        "plan_type": "pro",
                        "rate_limit": {"primary_window": {"used_percent": 100.0}},
                        "seen_account": acct,
                    }))
                }),
            )
            .route(
                "/backend-api/wham/rate-limit-reset-credits",
                get(|| async {
                    Json(json!({"credits":[
                        {"id":"expired","status":"available","reset_type":"codex_rate_limits","expires_at":"2000-01-01T00:00:00Z"},
                        {"id":"verify-credit","status":"available","reset_type":"codex_rate_limits","expires_at":"2099-01-01T00:00:00Z"}
                    ]}))
                }),
            )
            .route(
                "/backend-api/wham/rate-limit-reset-credits/consume",
                post({
                    let consumed = consumed.clone();
                    move |headers: HeaderMap, Json(raw): Json<Value>| {
                        let consumed = consumed.clone();
                        async move {
                            if router::bearer(&headers) != Some("cx-access-a")
                                || headers.get("chatgpt-account-id").and_then(|v| v.to_str().ok()) != Some("acct_verify_123")
                                || raw["credit_id"] != "verify-credit"
                                || !raw["redeem_request_id"]
                                    .as_str()
                                    .and_then(|id| Uuid::parse_str(id).ok())
                                    .is_some_and(|id| id.get_version_num() == 5)
                            {
                                return StatusCode::BAD_REQUEST.into_response();
                            }
                            consumed.fetch_add(1, Ordering::SeqCst);
                            Json(json!({"code":"reset"})).into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1internal:retrieveUserQuotaSummary",
                post(|headers: HeaderMap, body: String| async move {
                    let auth = router::bearer(&headers).unwrap_or_default().to_owned();
                    Json(json!({
                        "buckets": [{
                            "bucketId": "gemini-5h",
                            "window": "5h",
                            "remainingFraction": 0.8,
                            "resetTime": Utc::now() + TimeDelta::hours(2)
                        }],
                        "seen_token": auth,
                        "seen_body": body,
                    }))
                }),
            )
            .route(
                "/v1internal:fetchAvailableModels",
                post(|| async { Json(json!({"models":{"gemini-4-neon-high":{"quotaInfo":{"remainingFraction":0.8}}}})) }),
            ),
    )
    .await?;
    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[
            ("ag-a.json", google_cred("ag-access-a", "ag-refresh-a", "ag-a@example.test", "proj-alpha")),
            ("claude-a.json", claude_cred("cl-access-a", "cl-a@example.test")),
            ("codex-a.json", codex_cred("cx-access-a", "cx-a@example.test", "acct_verify_123")),
        ],
    )?;
    let accounts = proxy.state.read().await.pool.accounts.clone();
    check_management_allowlist(&accounts)?;
    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;

    // 1. /v0/management/auth-files contract + zero secret leakage
    let auth_res = http.get(server.url("/v0/management/auth-files")).bearer_auth("verify-usage-key").send().await?;
    ensure!(auth_res.status() == StatusCode::OK, "auth-files status {}", auth_res.status());
    let auth_text = auth_res.text().await?;
    for secret in ["ag-access-a", "ag-refresh-a", "verify-google-secret", "cl-access-a", "cx-access-a"] {
        ensure!(!auth_text.contains(secret), "management auth-files leaked secret {secret}");
    }
    let auth_json: Value = serde_json::from_str(&auth_text)?;
    let files = auth_json["files"].as_array().ok_or_else(|| anyhow!("missing files array"))?;
    ensure!(files.len() == 3, "expected 3 auth files, got {}", files.len());
    let ag = files.iter().find(|f| f["provider"] == "antigravity").ok_or_else(|| anyhow!("antigravity entry missing"))?;
    let cl = files.iter().find(|f| f["provider"] == "claude").ok_or_else(|| anyhow!("claude entry missing"))?;
    let cx = files.iter().find(|f| f["provider"] == "codex").ok_or_else(|| anyhow!("codex entry missing"))?;
    ensure!(ag["project_id"] == "proj-alpha", "antigravity project_id missing");
    ensure!(cx["id_token"]["chatgpt_account_id"] == "acct_verify_123", "codex id_token.chatgpt_account_id missing");

    // 2. /v0/management/api-call for Claude, Codex, and Antigravity
    for (label, auth_index, method, url, extra_headers, data) in [
        (
            "claude",
            cl["auth_index"].as_str().unwrap(),
            "GET",
            "https://api.anthropic.com/api/oauth/usage",
            json!({"anthropic-beta": "oauth-2025-04-20"}),
            None,
        ),
        (
            "codex",
            cx["auth_index"].as_str().unwrap(),
            "GET",
            "https://chatgpt.com/backend-api/wham/usage",
            json!({"Chatgpt-Account-Id": "acct_verify_123"}),
            None,
        ),
        (
            "antigravity",
            ag["auth_index"].as_str().unwrap(),
            "POST",
            "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
            json!({"Content-Type": "application/json", "User-Agent": "antigravity/hub/2.24.0"}),
            Some(r#"{"project":"proj-alpha"}"#),
        ),
    ] {
        let mut hdr = extra_headers.as_object().unwrap().clone();
        hdr.insert("Authorization".into(), json!("Bearer $TOKEN$"));
        let mut req = json!({
            "auth_index": auth_index,
            "method": method,
            "url": url,
            "header": hdr,
        });
        if let Some(body) = data {
            req["data"] = json!(body);
        }
        let res = http.post(server.url("/v0/management/api-call")).bearer_auth("verify-usage-key").json(&req).send().await?;
        ensure!(res.status() == StatusCode::OK, "api-call {label} failed with {}", res.status());
        let body: Value = res.json().await?;
        ensure!(body["status_code"] == 200, "api-call {label} upstream status {}", body["status_code"]);
    }

    // 3. Disallowed upstream SSRF rejection
    let ssrf = http
        .post(server.url("/v0/management/api-call"))
        .bearer_auth("verify-mgmt-key")
        .json(&json!({
            "auth_index": ag["auth_index"],
            "method": "GET",
            "url": "https://evil.example.invalid/steal",
            "header": {"Authorization": "Bearer $TOKEN$"},
        }))
        .send()
        .await?;
    ensure!(ssrf.status() == StatusCode::FORBIDDEN, "expected 403 for disallowed upstream, got {}", ssrf.status());

    // Native Google quota probing runs independently of T3's CLIProxy account filtering.
    proxy.refresh_usage(&accounts["ag-a.json"]).await.map_err(|err| anyhow!("{}", err.summary()))?;
    ensure!(proxy.state.read().await.pool.accounts["ag-a.json"].windows.iter().any(|w| w.used_percent.is_some()));

    let codex_cooldown = Cooldown { until: Utc::now() + TimeDelta::minutes(3), verified_reset: false };
    proxy.state.write().await.pool.accounts.get_mut("codex-a.json").unwrap().cooldown = Some(codex_cooldown);
    let real_t3 = if let Some(root) = std::env::var_os("T3CODE_VERIFY_ROOT") {
        let now = Utc::now();
        let model = "gemini-4-neon-high";
        proxy.state.write().await.pool.accounts.get_mut("ag-a.json").unwrap().cooldown =
            Some(Cooldown { until: now + TimeDelta::seconds(30), verified_reset: true });
        let res = http
            .post(server.url("/v1internal:generateContent"))
            .bearer_auth("verify-client-key")
            .json(&json!({"model":model}))
            .send()
            .await?;
        let witness = res.headers().get(router::POOL_QUOTA_HEADER).context("missing real T3 fixture witness")?.to_str()?.to_owned();
        let body = res.json::<Value>().await?;
        check_real_t3code(PathBuf::from(root), &dir, &server, &mock, now, &witness, &body).await?;
        "real T3 source verified Claude/Codex/Antigravity reads, Codex credit redemption, and verified pool quota arm/dispatch"
    } else {
        let call = |method, path, data| {
            http.post(server.url("/v0/management/api-call")).bearer_auth("verify-mgmt-key").json(&json!({
                "auth_index":cx["auth_index"], "method":method, "url":mock.url(path), "data":data,
                "header":{"Authorization":"Bearer $TOKEN$","Chatgpt-Account-Id":"acct_verify_123","Content-Type":"application/json"},
            }))
        };
        let path = "/backend-api/wham/rate-limit-reset-credits";
        let credits: Value = call("GET", path, None).send().await?.error_for_status()?.json().await?;
        ensure!(credits["status_code"] == 200);
        let body: Value = serde_json::from_str(credits["body"].as_str().context("missing credit body")?)?;
        ensure!(body["credits"][1]["id"] == "verify-credit");
        let redeemed: Value = call(
            "POST",
            &format!("{path}/consume"),
            Some(
                json!({
                    "credit_id":"verify-credit", "redeem_request_id":"d672e747-4211-53b8-8d01-303be0119b17"
                })
                .to_string(),
            ),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
        ensure!(redeemed["status_code"] == 200);
        ensure!(serde_json::from_str::<Value>(redeemed["body"].as_str().context("missing consume body")?)?["code"] == "reset");
        ensure!(proxy.state.read().await.pool.accounts["codex-a.json"].cooldown == Some(codex_cooldown));
        http.post(server.url("/v0/management/reset-quota"))
            .bearer_auth("verify-mgmt-key")
            .json(&json!({"auth_index": cx["auth_index"]}))
            .send()
            .await?
            .error_for_status()?;
        "mock Codex credit check, consume and reset-quota verified; set T3CODE_VERIFY_ROOT to run the real T3 client and recovery worker"
    };
    ensure!(consumed.load(Ordering::SeqCst) == 1 && proxy.state.read().await.pool.accounts["codex-a.json"].cooldown.is_none());

    let windows = http.get(server.url("/v8/management/usage/windows")).bearer_auth("verify-mgmt-key").send().await?;
    ensure!(windows.status() == StatusCode::OK, "v8 usage/windows failed: {}", windows.status());
    let policy = http.get(server.url("/v8/management/routing/policy")).bearer_auth("verify-mgmt-key").send().await?;
    ensure!(policy.status() == StatusCode::OK, "v8 routing/policy failed: {}", policy.status());
    let pref = http
        .put(server.url("/v8/management/routing/preferred-auth"))
        .bearer_auth("verify-mgmt-key")
        .json(&json!({"provider": "antigravity", "account_id": "ag-a.json"}))
        .send()
        .await?;
    ensure!(pref.status() == StatusCode::OK, "v8 preferred-auth failed: {}", pref.status());
    proxy.state.write().await.pool.bind(crate::pool::Lane::Google, "verify-sess-1", "ag-a.json");
    let moved = http
        .post(server.url("/v8/management/routing/move-session"))
        .bearer_auth("verify-mgmt-key")
        .json(&json!({"session_id": "verify-sess-1", "account_id": "ag-a.json"}))
        .send()
        .await?;
    ensure!(moved.status() == StatusCode::OK, "v8 move-session failed: {}", moved.status());

    Ok(format!("production URL allowlist, SSRF guard, native Google probes, auth-files and v8 routing verified; {real_t3}"))
}

fn check_management_allowlist(accounts: &BTreeMap<String, crate::pool::Account>) -> Result<()> {
    let cl = &accounts["claude-a.json"];
    let cx = &accounts["codex-a.json"];
    let ag = &accounts["ag-a.json"];
    let allowed = |account: &crate::pool::Account, method: &str, url: &str| -> Result<bool> {
        Ok(management::allowed_call(account, &reqwest::Url::parse(url)?, method))
    };
    const ANTHROPIC: &str = "https://api.anthropic.com/api/oauth/usage";
    const WHAM: &str = "https://chatgpt.com/backend-api/wham/usage";
    for (account, method, url) in [
        (cl, "GET", ANTHROPIC),
        (cx, "GET", WHAM),
        (cx, "GET", "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits"),
        (cx, "POST", "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume"),
    ] {
        ensure!(allowed(account, method, url)?, "production allowlist rejected {method} {url}");
        ensure!(!allowed(ag, method, url)?, "production allowlist accepted the wrong lane");
    }
    for host in ["daily-cloudcode-pa.googleapis.com", "cloudcode-pa.googleapis.com"] {
        for path in ["retrieveUserQuotaSummary", "fetchAvailableModels"] {
            let url = format!("https://{host}/v1internal:{path}");
            ensure!(allowed(ag, "POST", &url)? && !allowed(ag, "GET", &url)? && !allowed(cl, "POST", &url)?);
        }
    }
    let mut key = cx.clone();
    key.api_key = true;
    for (account, method, url) in [
        (&key, "GET", WHAM),
        (cx, "POST", WHAM),
        (cl, "PUT", ANTHROPIC),
        (cx, "GET", "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume"),
        (cx, "GET", "https://chatgpt.com/backend-api/wham/usage?unexpected=1"),
        (cx, "GET", "https://chatgpt.com/backend-api/wham/usage#fragment"),
        (cx, "GET", "https://chatgpt.com:8443/backend-api/wham/usage"),
        (cx, "GET", "http://chatgpt.com/backend-api/wham/usage"),
        (cx, "GET", "https://chatgpt.com@127.0.0.1/backend-api/wham/usage"),
        (cx, "GET", "https://name:password@chatgpt.com/backend-api/wham/usage"),
        (cx, "GET", "https://chatgpt.com.example.invalid/backend-api/wham/usage"),
        (ag, "POST", "https://169.254.169.254/v1internal:retrieveUserQuotaSummary"),
    ] {
        ensure!(!allowed(account, method, url)?, "production allowlist accepted {method} {url}");
    }
    Ok(())
}

async fn check_real_t3code(
    root: PathBuf,
    dir: &TempDir,
    server: &EphemeralServer,
    mock: &EphemeralServer,
    now: DateTime<Utc>,
    witness: &str,
    body: &Value,
) -> Result<()> {
    let mut native = message_less_rate_limit();
    native["error"]["details"].as_array_mut().unwrap().push(json!({"@type":"google.rpc.RetryInfo","retryDelay":"30s"}));
    router::normalize_google_quota(&mut native, "gemini-4-neon-high", now);
    let mut node = tokio::process::Command::new("node");
    node.current_dir(root.join("apps/server"))
        .kill_on_drop(true)
        .args([
            "--experimental-transform-types",
            "--conditions=source",
            "--input-type=module",
            "-e",
            include_str!("verify-t3code.mjs"),
            "--",
        ])
        .arg(dir.path())
        .arg(server.url(""))
        .arg(mock.url(""))
        .arg(now.timestamp_millis().to_string())
        .arg(witness)
        .arg(body.to_string())
        .arg(render_go_genai_api_error(&native)?);
    let output = tokio::time::timeout(Duration::from_secs(25), node.output())
        .await
        .context("T3 source check timed out")?
        .context("T3 source check needs Node and a dependency-installed T3CODE_VERIFY_ROOT")?;
    ensure!(output.status.success(), "T3 source check: {}", String::from_utf8_lossy(&output.stderr));
    Ok(())
}

async fn check_antigravity_cloudcode_endpoints() -> Result<String> {
    let dir = TempDir::new()?;
    let seen_paths = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
    let mock = EphemeralServer::start(Router::new().fallback({
        let seen_paths = seen_paths.clone();
        move |req: axum::extract::Request| {
            let seen_paths = seen_paths.clone();
            async move {
                let path = req.uri().path_and_query().map(|pq| pq.as_str().to_owned()).unwrap_or_default();
                let has_meta = req.headers().contains_key("client-metadata")
                    && req.headers().contains_key("x-goog-api-client")
                    && req.headers().contains_key("x-client-request-id");
                seen_paths.lock().await.push((path.clone(), has_meta));
                if path.contains("alt=sse") {
                    let sse = concat!(
                        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]}}],",
                        "\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":12,\"cachedContentTokenCount\":80}}}\n\n"
                    );
                    (StatusCode::OK, [("content-type", "text/event-stream")], sse.to_owned()).into_response()
                } else {
                    (StatusCode::OK, Json(json!({"ok": true, "path": path}))).into_response()
                }
            }
        }
    }))
    .await?;
    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[("ag-a.json", google_cred("ag-access-a", "ag-refresh-a", "ag-a@example.test", "proj-alpha"))],
    )?;
    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;

    let endpoints = [
        ("/v1internal:loadCodeAssist", json!({"metadata": {"ideType": "ANTIGRAVITY"}})),
        ("/v1internal:onboardUser", json!({"tierId": "standard"})),
        ("/v1internal:fetchAvailableModels", json!({"project": "proj-alpha"})),
        ("/v1internal:retrieveUserQuotaSummary", json!({"project": "proj-alpha"})),
        ("/v1internal:countTokens", json!({"request": {"model": "models/gemini-4-neon-high", "contents": []}})),
        ("/v1internal:generateContent", json!({"model": "gemini-4-neon-high", "project": "proj-alpha", "request": {"contents": []}})),
        (
            "/v1internal:streamGenerateContent?alt=sse",
            json!({"model": "gemini-4-neon-high", "project": "proj-alpha", "request": {"contents": []}}),
        ),
        ("/v1beta/models/gemini-4-neon-high:streamGenerateContent?alt=sse", json!({"contents": []})),
    ];

    for (path, payload) in &endpoints {
        let res = http
            .post(server.url(path))
            .bearer_auth("verify-client-key")
            .header("client-metadata", "ideType=ANTIGRAVITY")
            .header("x-goog-api-client", "gl-go/1.24.0")
            .header("x-client-request-id", "req-verify-1")
            .json(payload)
            .send()
            .await?;
        ensure!(res.status() == StatusCode::OK, "endpoint {path} returned {}", res.status());
        let _ = res.bytes().await?;
    }
    let duplicate = http
        .post(server.url("/v1internal:generateContent"))
        .bearer_auth("verify-client-key")
        .body(br#"{"project":"proj-alpha","pro\u006aect":"proj-beta"}"#.as_slice())
        .send()
        .await?;
    ensure!(duplicate.status() == StatusCode::BAD_REQUEST, "escaped duplicate project should be rejected before dispatch");

    let seen = seen_paths.lock().await;
    ensure!(seen.len() == endpoints.len(), "expected {} upstream calls, saw {}", endpoints.len(), seen.len());
    ensure!(seen.iter().all(|(_, meta)| *meta), "expected Google metadata headers forwarded on all Cloud Code endpoints");
    let state = proxy.state.read().await;
    let count_tokens_model =
        state.activity.iter().find(|row| row.requested_model == "gemini-4-neon-high").map(|row| row.requested_model.as_str());
    ensure!(count_tokens_model == Some("gemini-4-neon-high"), "expected normalized model without models/ prefix");
    Ok(format!("all {} Antigravity Cloud Code & v1beta endpoints forwarded with Google headers intact", endpoints.len()))
}

async fn check_antigravity_mcp_tooling_and_streaming() -> Result<String> {
    let dir = TempDir::new()?;
    let captured_bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let turn = Arc::new(AtomicUsize::new(0));
    let turn1_sse = concat!(
        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
        "{\"thought\":true,\"text\":\"Checking PR state via t3-code MCP...\"},",
        "{\"thoughtSignature\":\"sig_v1_abc123\",\"functionCall\":{\"name\":\"mcp_t3-code_pull_request_read\",\"args\":{\"include\":[\"checks\",\"threads\"]}}}",
        "]}}],\"usageMetadata\":{\"promptTokenCount\":4200,\"candidatesTokenCount\":140,\"thoughtsTokenCount\":64,\"cachedContentTokenCount\":3200}}}\n\n"
    );
    let turn2_sse = concat!(
        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[",
        "{\"text\":\"All required PR checks passed.\"}",
        "]}}],\"usageMetadata\":{\"promptTokenCount\":4600,\"candidatesTokenCount\":48,\"thoughtsTokenCount\":12,\"cachedContentTokenCount\":4100}}}\n\n"
    );
    let mock = EphemeralServer::start(Router::new().route(
        "/v1internal:streamGenerateContent",
        post({
            let captured_bodies = captured_bodies.clone();
            let turn = turn.clone();
            move |body: String| {
                let captured_bodies = captured_bodies.clone();
                let turn = turn.clone();
                async move {
                    captured_bodies.lock().await.push(body);
                    let idx = turn.fetch_add(1, Ordering::SeqCst);
                    let payload = if idx == 0 { turn1_sse } else { turn2_sse };
                    (StatusCode::OK, [("content-type", "text/event-stream")], payload.to_owned())
                }
            }
        }),
    ))
    .await?;

    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[("ag-a.json", google_cred("ag-access-a", "ag-refresh-a", "ag-a@example.test", "proj-alpha"))],
    )?;
    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;

    // Deliberately non-alphabetical keys in `request` (z_tool, a_tool, tools, systemInstruction, contents)
    // to prove `replace_top_level_json_string` rewrites ONLY top-level `project` and preserves `request` byte-for-byte.
    let request_block_turn1 = r#"{"z_marker":"keep-order","tools":[{"functionDeclarations":[{"name":"mcp_t3-code_pull_request_read","parameters":{"type":"OBJECT","properties":{"z_param":{"type":"STRING"},"a_param":{"type":"BOOLEAN"}}}}]}],"systemInstruction":{"parts":[{"text":"You are Antigravity in T3 Code."}]},"contents":[{"role":"user","parts":[{"text":"Check PR checks"}]}]}"#;
    let body_turn1 = format!(r#"{{"model":"gemini-4-neon-high","project":"stale-client-project","request":{request_block_turn1}}}"#);

    let res1 = http
        .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
        .bearer_auth("verify-client-key")
        .header("content-type", "application/json")
        .body(body_turn1)
        .send()
        .await?;
    ensure!(res1.status() == StatusCode::OK, "turn 1 status {}", res1.status());
    let text1 = res1.text().await?;
    ensure!(text1 == turn1_sse, "turn 1 SSE stream was modified in transit");

    let request_block_turn2 = r#"{"z_marker":"keep-order","tools":[{"functionDeclarations":[{"name":"mcp_t3-code_pull_request_read","parameters":{"type":"OBJECT","properties":{"z_param":{"type":"STRING"},"a_param":{"type":"BOOLEAN"}}}}]}],"systemInstruction":{"parts":[{"text":"You are Antigravity in T3 Code."}]},"contents":[{"role":"user","parts":[{"text":"Check PR checks"}]},{"role":"model","parts":[{"thoughtSignature":"sig_v1_abc123","functionCall":{"name":"mcp_t3-code_pull_request_read","args":{"include":["checks","threads"]}}}]},{"role":"user","parts":[{"functionResponse":{"name":"mcp_t3-code_pull_request_read","response":{"status":"passed"}}}]}]}"#;
    let body_turn2 = format!(r#"{{"model":"gemini-4-neon-high","project":"stale-client-project","request":{request_block_turn2}}}"#);

    let res2 = http
        .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
        .bearer_auth("verify-client-key")
        .header("content-type", "application/json")
        .body(body_turn2)
        .send()
        .await?;
    ensure!(res2.status() == StatusCode::OK, "turn 2 status {}", res2.status());
    let text2 = res2.text().await?;
    ensure!(text2 == turn2_sse, "turn 2 SSE stream was modified in transit");

    let captured = captured_bodies.lock().await;
    ensure!(captured.len() == 2, "expected 2 upstream requests");
    ensure!(captured[0].contains(r#""project":"proj-alpha""#), "top-level project was not rewritten");
    ensure!(captured[0].contains(request_block_turn1), "turn 1 request payload bytes/key order changed during project rewrite");
    ensure!(captured[1].contains(request_block_turn2), "turn 2 request payload bytes/key order changed during project rewrite");

    // Yield briefly for Wire::next drop/completion recording to settle
    tokio::time::sleep(Duration::from_millis(25)).await;
    let state = proxy.state.read().await;
    ensure!(state.activity.len() == 2, "expected 2 recorded exchanges, got {}", state.activity.len());
    ensure!(state.activity[0].cache_read_tokens == 3200, "turn 1 cache_read_tokens mismatch");
    ensure!(state.activity[0].thinking_tokens == 64, "turn 1 thinking_tokens mismatch");
    ensure!(state.activity[1].cache_read_tokens == 4100, "turn 2 cache_read_tokens mismatch");
    ensure!(state.activity.iter().all(|row| row.session_id.is_none()), "derived cache routing leaked into trace session IDs");

    Ok("2-turn MCP tool-call SSE stream (thoughtSignature + functionCall/Response) preserved byte-for-byte; request JSON key order preserved on project rewrite".into())
}

async fn check_antigravity_cache_hit_preservation() -> Result<String> {
    let dir = TempDir::new()?;
    let fail_alpha = Arc::new(AtomicBool::new(false));
    let route_log = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let mock = EphemeralServer::start(Router::new().route(
        "/v1internal:streamGenerateContent",
        post({
            let fail_alpha = fail_alpha.clone();
            let route_log = route_log.clone();
            move |headers: HeaderMap, Json(body): Json<Value>| {
                let fail_alpha = fail_alpha.clone();
                let route_log = route_log.clone();
                async move {
                    let token = router::bearer(&headers).unwrap_or_default().to_owned();
                    let project = body["project"].as_str().unwrap_or_default().to_owned();
                    route_log.lock().await.push((token.clone(), project.clone()));
                    if body.pointer("/request/cachedContent").or_else(|| body.get("cachedContent")).is_some()
                        && (token != "ag-access-a" || project != "proj-alpha")
                    {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    if token == "ag-access-a" && fail_alpha.load(Ordering::SeqCst) {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({
                                "error": {
                                    "code": 429,
                                    "message": "Quota exhausted",
                                    "status": "RESOURCE_EXHAUSTED",
                                    "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "60s"}]
                                }
                            })),
                        )
                            .into_response();
                    }
                    let turns = body.pointer("/request/contents").and_then(Value::as_array).map_or(1, Vec::len);
                    let cached = if turns > 1 { 8_000 } else { 0 };
                    let sse = format!(
                        "data: {{\"response\":{{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"ok\"}}]}}}}],\"usageMetadata\":{{\"promptTokenCount\":10000,\"candidatesTokenCount\":50,\"cachedContentTokenCount\":{cached}}}}} }}\n\n"
                    );
                    (StatusCode::OK, [("content-type", "text/event-stream")], sse).into_response()
                }
            }
        }),
    ))
    .await?;

    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[
            ("ag-a.json", google_cred("ag-access-a", "ag-refresh-a", "ag-a@example.test", "proj-alpha")),
            ("ag-b.json", google_cred("ag-access-b", "ag-refresh-b", "ag-b@example.test", "proj-beta")),
        ],
    )?;
    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;

    // Verify sticky cache affinity across all strategies using real agy shape:
    // shared numeric request.session_id, NO x-session-id header, growing contents.
    for strategy in [
        RouteStrategy::RoundRobin,
        RouteStrategy::WeightedRoundRobin,
        RouteStrategy::SoonestReset,
        RouteStrategy::FillFirst,
        RouteStrategy::CapacityBalanced,
    ] {
        {
            let mut state = proxy.state.write().await;
            state.pool.policy.strategy = strategy;
            state.pool.clear_affinity();
        }
        route_log.lock().await.clear();

        let thread_a_t1 = json!({
            "model": "gemini-4-neon-high",
            "project": "client-default",
            "request": {
                "session_id": "-3750763034362895579",
                "systemInstruction": {"parts": [{"text": "System prompt A"}]},
                "contents": [{"role": "user", "parts": [{"text": format!("Thread A prompt ({strategy:?})")}]}]
            }
        });
        let thread_a_t2 = json!({
            "model": "gemini-4-neon-high",
            "project": "client-default",
            "request": {
                "session_id": "-3750763034362895579",
                "systemInstruction": {"parts": [{"text": "System prompt A"}]},
                "contents": [
                    {"role": "user", "parts": [{"text": format!("Thread A prompt ({strategy:?})")}]},
                    {"role": "model", "parts": [{"text": "Reply 1"}]},
                    {"role": "user", "parts": [{"text": "Follow up 2"}]}
                ]
            }
        });
        let thread_b_t1 = json!({
            "model": "gemini-4-neon-high",
            "project": "client-default",
            "request": {
                "session_id": "-3750763034362895579",
                "systemInstruction": {"parts": [{"text": "System prompt A"}]},
                "contents": [{"role": "user", "parts": [{"text": format!("Thread B distinct prompt ({strategy:?})")}]}]
            }
        });

        for payload in [&thread_a_t1, &thread_a_t2, &thread_b_t1] {
            let res = http
                .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
                .bearer_auth("verify-client-key")
                .json(payload)
                .send()
                .await?;
            ensure!(res.status() == StatusCode::OK, "strategy {strategy:?} failed with {}", res.status());
            let _ = res.text().await?;
        }
        let log = route_log.lock().await;
        ensure!(log.len() == 3, "expected 3 routed calls under {strategy:?}");
        ensure!(log[0] == log[1], "strategy {strategy:?} broke cache affinity between turn 1 ({:?}) and turn 2 ({:?})", log[0], log[1]);
        if strategy == RouteStrategy::RoundRobin {
            ensure!(log[2].0 != log[0].0, "RoundRobin should balance distinct Thread B onto second account (both went to {})", log[0].0);
        }
    }

    // A preferred account from another project must never see this cache name.
    {
        let mut state = proxy.state.write().await;
        state.pool.policy.strategy = RouteStrategy::RoundRobin;
        state.pool.clear_affinity();
        state.pool.overrides.insert(crate::pool::Lane::Google, "ag-b.json".into());
    }
    route_log.lock().await.clear();
    let cached_req = json!({
        "model": "gemini-4-neon-high",
        "project": "client-default",
        "request": {
            "cachedContent": "projects/proj-alpha/locations/global/cachedContents/cache_999",
            "contents": [{"role": "user", "parts": [{"text": "Turn 1 using explicit cache"}]}]
        }
    });
    for _ in 0..2 {
        let res = http
            .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
            .bearer_auth("verify-client-key")
            .json(&cached_req)
            .send()
            .await?;
        ensure!(res.status() == StatusCode::OK);
        let _ = res.text().await?;
    }
    fail_alpha.store(true, Ordering::SeqCst);
    for payload in [
        &cached_req,
        &json!({
            "model": "gemini-4-neon-high",
            "project": "client-default",
            "cachedContent": cached_req["request"]["cachedContent"],
        }),
    ] {
        let res = http
            .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
            .bearer_auth("verify-client-key")
            .json(payload)
            .send()
            .await?;
        ensure!(res.status() == StatusCode::TOO_MANY_REQUESTS, "cache owner quota failure returned {}", res.status());
        let reset = check_google_pool_error(res, "gemini-4-neon-high").await?;
        ensure!(reset.is_some_and(|at| (55..=65).contains(&(at - Utc::now()).num_seconds())), "expected the cache owner's reset");
    }
    let log = route_log.lock().await;
    ensure!(log.len() == 3 && log.iter().all(|(token, project)| token == "ag-access-a" && project == "proj-alpha"));
    Ok("all four strategies preserve cache affinity; cachedContent stays on its owning project on first pick and on 429".into())
}

async fn check_google_pool_error(response: reqwest::Response, model: &str) -> Result<Option<DateTime<Utc>>> {
    ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
    ensure!(response.headers().get("x-should-retry").and_then(|v| v.to_str().ok()) == Some("false"));
    let mut witnesses = response.headers().get_all(router::POOL_QUOTA_HEADER).iter();
    let reset = if let Some(witness) = witnesses.next() {
        ensure!(witnesses.next().is_none(), "duplicated pool quota witness");
        let (stamp, wire_model) =
            witness.to_str()?.strip_prefix("v1;retry_at=").and_then(|s| s.split_once(";wire_model=")).context("bad pool quota witness")?;
        ensure!(wire_model == model && stamp.ends_with('Z') && !stamp.contains('.'), "bad pool quota witness: {witness:?}");
        Some(stamp.parse()?)
    } else {
        None
    };
    let body: Value = response.json().await?;
    let error = &body["error"];
    ensure!(error["code"] == 429 && error["status"] == "RESOURCE_EXHAUSTED");
    ensure!(error["message"] == "all eligible accounts are rate limited");
    ensure!(
        error["details"]
            == json!([{
                "@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED",
                "domain":"cloudcode-pa.googleapis.com","metadata":{"model":model}
            }]),
        "unexpected pool quota details: {}",
        error["details"]
    );
    Ok(reset)
}

async fn check_antigravity_quota_exhaustion_and_thread_auto_resume() -> Result<String> {
    let dir = TempDir::new()?;
    let upstream_429 = Arc::new(AtomicBool::new(true));
    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let mock = EphemeralServer::start(Router::new().route(
        "/v1internal:streamGenerateContent",
        post({
            let upstream_429 = upstream_429.clone();
            let upstream_calls = upstream_calls.clone();
            move |headers: HeaderMap| {
                let upstream_429 = upstream_429.clone();
                let upstream_calls = upstream_calls.clone();
                async move {
                    upstream_calls.fetch_add(1, Ordering::SeqCst);
                    if upstream_429.load(Ordering::SeqCst) {
                        let detail = if router::bearer(&headers) == Some("ag-access-a") {
                            json!({"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "45s"})
                        } else {
                            json!({
                                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                                "reason": "QUOTA_EXHAUSTED",
                                "metadata": {"quotaResetDelay": "2h15m3s"}
                            })
                        };
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("retry-after", "1")],
                            Json(json!({
                                "error": {
                                    "code": 429,
                                    "message": "Upstream exhausted",
                                    "status": "RESOURCE_EXHAUSTED",
                                    "details": [detail]
                                }
                            })),
                        )
                            .into_response();
                    }
                    let sse = concat!(
                        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Resumed after quota reset!\"}]}}],",
                        "\"usageMetadata\":{\"promptTokenCount\":2000,\"candidatesTokenCount\":32,\"cachedContentTokenCount\":1500}}}\n\n"
                    );
                    (StatusCode::OK, [("content-type", "text/event-stream")], sse.to_owned()).into_response()
                }
            }
        }),
    ))
    .await?;

    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[
            ("ag-a.json", google_cred("ag-access-a", "ag-refresh-a", "ag-a@example.test", "proj-alpha")),
            ("ag-b.json", google_cred("ag-access-b", "ag-refresh-b", "ag-b@example.test", "proj-beta")),
        ],
    )?;

    // Path A: Pre-exhausted quota pool (e.g. 3p-weekly at 100% on every account)
    let now_a = Utc::now();
    let reset_a = DateTime::from_timestamp((now_a + TimeDelta::seconds(180)).timestamp(), 0).unwrap();
    let reset_b = DateTime::from_timestamp((now_a + TimeDelta::seconds(7200)).timestamp(), 0).unwrap();
    {
        let mut state = proxy.state.write().await;
        for (id, reset) in [("ag-a.json", reset_a), ("ag-b.json", reset_b)] {
            if let Some(acct) = state.pool.accounts.get_mut(id) {
                acct.windows = vec![QuotaWindow {
                    window_key: "3p-weekly".into(),
                    label: "Claude / GPT weekly".into(),
                    kind: QuotaWindowKind::ModelScoped,
                    scope_model: Some("claude-*".into()),
                    unit: QuotaUnit::Percent,
                    used_percent: Some(100.0),
                    remaining_percent: Some(0.0),
                    used_amount: None,
                    limit_amount: None,
                    currency: None,
                    resets_at: Some(reset),
                    window_duration_mins: Some(10080),
                    cadence: ResetCadence::Fixed,
                    status: QuotaStatus::Rejected,
                    observation_state: QuotaObservationState::Measured,
                    observed_at: Some(now_a),
                    stale: false,
                    error_reason: None,
                }];
            }
        }
    }

    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;

    let payload = json!({
        "model": "claude-sonnet-4-6",
        "project": "client-default",
        "request": {"contents": [{"role": "user", "parts": [{"text": "Continue implementation"}]}]}
    });
    let res_a =
        http.post(server.url("/v1internal:streamGenerateContent?alt=sse")).bearer_auth("verify-client-key").json(&payload).send().await?;
    ensure!(upstream_calls.load(Ordering::SeqCst) == 0, "pre-exhausted pool should not hit upstream");
    ensure!(check_google_pool_error(res_a, "claude-sonnet-4-6").await? == Some(reset_a), "Path A did not witness the earliest reset");

    // Path B: Dynamic upstream 429 exhaustion across all accounts
    {
        let mut state = proxy.state.write().await;
        for acct in state.pool.accounts.values_mut() {
            acct.windows.clear();
            acct.scoped_cooldowns.clear();
            acct.cooldown = None;
        }
    }
    let now_b = Utc::now();
    let res_b =
        http.post(server.url("/v1internal:streamGenerateContent?alt=sse")).bearer_auth("verify-client-key").json(&payload).send().await?;
    let parsed_reset_b = check_google_pool_error(res_b, "claude-sonnet-4-6").await?.context("Path B missing pool quota witness")?;
    let delta_b = (parsed_reset_b - now_b).num_seconds();
    ensure!((40..=50).contains(&delta_b), "Path B reset delay should match earliest ~45s reset, got {delta_b}s");

    // Path C: the only quota-limited account must still produce a recoverable reset
    // when another supported credential is cooling for an unrelated auth failure.
    {
        let mut state = proxy.state.write().await;
        state.pool.accounts.get_mut("ag-a.json").unwrap().scoped_cooldowns.clear();
        let beta = state.pool.accounts.get_mut("ag-b.json").unwrap();
        beta.scoped_cooldowns.clear();
        beta.record_error_once(&local_error(AccountErrorKind::AuthExpired, AccountErrorSource::Inference, "fixture auth failure"));
    }
    let calls_before_mixed = upstream_calls.load(Ordering::SeqCst);
    for attempt in 1..=2 {
        let mixed = http
            .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
            .bearer_auth("verify-client-key")
            .json(&payload)
            .send()
            .await?;
        let mixed_reset = check_google_pool_error(mixed, "claude-sonnet-4-6").await?;
        ensure!(
            mixed_reset.is_some_and(|at| (40..=50).contains(&(at - Utc::now()).num_seconds())),
            "mixed pool request {attempt}: missing verified reset"
        );
        ensure!(upstream_calls.load(Ordering::SeqCst) == calls_before_mixed + 1);
    }

    // Verify the proxy accepts the continuation once quota is reset. The real
    // worker's decision to dispatch is checked separately with T3CODE_VERIFY_ROOT.
    upstream_429.store(false, Ordering::SeqCst);
    {
        let mut state = proxy.state.write().await;
        if let Some(acct) = state.pool.accounts.get_mut("ag-a.json") {
            acct.scoped_cooldowns.clear();
            acct.cooldown = None;
            acct.windows.clear();
        }
    }
    let resumed = http
        .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
        .bearer_auth("verify-client-key")
        .json(&json!({
            "model": "claude-sonnet-4-6",
            "project": "client-default",
            "request": {
                "contents": [
                    {"role": "user", "parts": [{"text": "Continue implementation"}]},
                    {"role": "user", "parts": [{"text": "Continue."}]}
                ]
            }
        }))
        .send()
        .await?;
    ensure!(resumed.status() == StatusCode::OK, "auto-resumed turn expected 200, got {}", resumed.status());
    let resumed_text = resumed.text().await?;
    ensure!(resumed_text.contains("Resumed after quota reset!"), "resumed response missing expected body");

    Ok("pre-exhausted, dynamic and mixed-pool 429s produce recoverable Google errors; continuation succeeds after reset".into())
}

async fn check_antigravity_in_band_errors() -> Result<String> {
    const CALL: &str = r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"verify","args":{}}}]}}]}}"#;
    const ERROR: &str = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota exhausted"}}"#;
    const AUTH: &str = r#"{"error":{"status":"UNAUTHENTICATED","message":"token rejected"}}"#;
    const UNAVAILABLE: &str = r#"{"error":{"code":503,"status":"UNAVAILABLE","message":"upstream unavailable"}}"#;
    let dir = TempDir::new()?;
    let mock = EphemeralServer::start(Router::new().route(
        "/v1internal:streamGenerateContent",
        post(|headers: HeaderMap, Json(request): Json<Value>| async move {
            if router::bearer(&headers) == Some("ag-access-b") {
                return ([("content-type", "application/json")], CALL).into_response();
            }
            let meta = r#"{"response":{"usageMetadata":{"promptTokenCount":8}}}"#;
            let case = request["case"].as_str().unwrap();
            let (kind, prefix, suffix) = match case {
                "early-object" | "early-auth" => ("application/json", String::new(), String::new()),
                "early-array" => ("application/json", format!("[{meta},"), "]".into()),
                "early-sse" => ("text/event-stream", format!("data: {meta}\n\ndata: "), "\n\n".into()),
                "mid-array" => ("application/json", format!("[{CALL},\n"), "]".into()),
                _ => ("text/event-stream", format!("data: {CALL}\r\n\r\ndata: "), "\r\n\r\n".into()),
            };
            let error = match case {
                "early-auth" => AUTH,
                "mid-http" => UNAVAILABLE,
                _ => ERROR,
            };
            let split = error.len() / 2;
            let chunks = [prefix + &error[..split], error[split..].to_owned() + &suffix];
            let body = axum::body::Body::from_stream(futures_util::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)));
            ([("content-type", kind)], body).into_response()
        }),
    ))
    .await?;
    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[
            ("ag-a.json", google_cred("ag-access-a", "ag-refresh-a", "ag-a@example.test", "proj-alpha")),
            ("ag-b.json", google_cred("ag-access-b", "ag-refresh-b", "ag-b@example.test", "proj-beta")),
        ],
    )?;
    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;
    for (index, (case, code)) in [
        ("early-object", 429),
        ("early-array", 429),
        ("early-sse", 429),
        ("early-auth", 401),
        ("mid-array", 429),
        ("mid-sse", 429),
        ("mid-http", 503),
        ("exhausted-sse", 429),
    ]
    .into_iter()
    .enumerate()
    {
        {
            let mut state = proxy.state.write().await;
            let pool = &mut state.pool;
            let alpha = pool.accounts.get_mut("ag-a.json").unwrap();
            alpha.scoped_cooldowns.clear();
            alpha.clear_account_error();
            if case == "exhausted-sse" {
                pool.accounts.get_mut("ag-b.json").unwrap().record_error_once(&local_error(
                    AccountErrorKind::AuthExpired,
                    AccountErrorSource::Inference,
                    "fixture auth failure",
                ));
            }
            pool.clear_affinity();
            pool.overrides.insert(crate::pool::Lane::Google, "ag-a.json".into());
        }
        let response = http
            .post(server.url("/v1internal:streamGenerateContent"))
            .bearer_auth("verify-client-key")
            .header("x-session-id", "verify-in-band")
            .json(&json!({"model":"gemini-4-neon-high","case":if case == "exhausted-sse" { "early-sse" } else { case }}))
            .send()
            .await?;
        let early = case.starts_with("early");
        let mid = case.starts_with("mid");
        ensure!(response.status() == if early || mid { StatusCode::OK } else { StatusCode::TOO_MANY_REQUESTS }, "{case}: status");
        ensure!(!response.headers().contains_key(router::POOL_QUOTA_HEADER), "{case}: unexpected witness");
        let body = response.text().await?;
        if early {
            ensure!(body == CALL, "{case}: data from the failed account escaped");
        } else if code == 429 {
            let error = match case {
                "mid-array" => {
                    ensure!(body.starts_with(&format!("[{CALL},\n")));
                    serde_json::from_str::<Value>(&body)?[1].clone()
                }
                "mid-sse" => {
                    ensure!(body.starts_with(&format!("data: {CALL}\r\n\r\n")));
                    ensure!(body.ends_with("\r\n\r\n"));
                    serde_json::from_str(body.lines().nth(2).unwrap().strip_prefix("data: ").unwrap())?
                }
                _ => serde_json::from_str(&body)?,
            };
            ensure!(error["error"]["status"] == "RESOURCE_EXHAUSTED");
            ensure!(error["error"]["details"][0]["reason"] == "QUOTA_EXHAUSTED");
            ensure!(!error.to_string().contains("quotaReset") && !error.to_string().contains("retryDelay"), "{case}: fabricated reset");
        } else {
            ensure!(body == format!("data: {CALL}\r\n\r\ndata: {UNAVAILABLE}\r\n\r\n"));
        }
        let state = proxy.state.read().await;
        let row = state.activity.back().context("missing activity")?;
        let failed = if code == 429 { "rate_limited" } else { "http_error" };
        ensure!(row.outcome == if early { "success" } else { failed }, "{case}: {}", row.outcome);
        ensure!(row.attempts.len() == if early { 2 } else { 1 }, "{case}: replayed or missing attempt");
        ensure!(row.attempts[0].outcome == failed && row.attempts[0].status_code == Some(code));
        let alpha = &state.pool.accounts["ag-a.json"];
        ensure!(alpha.failure == index as u64 + 1 && alpha.success == 0);
        ensure!(alpha.scoped_cooldowns.contains_key("gemini") == (code == 429));
        ensure!(alpha.quarantined(Utc::now()) == (code == 401));
        if !mid {
            ensure!(state.pool.bound(crate::pool::Lane::Google, "verify-in-band") == early.then_some("ag-b.json"));
        }
    }
    Ok("HTTP 200 object, array and SSE errors fail over only before output; streamed calls survive quota errors without replay".into())
}

async fn check_antigravity_dynamic_auth_and_401_recovery() -> Result<String> {
    let dir = TempDir::new()?;
    let tokeninfo_hits = Arc::new(AtomicUsize::new(0));
    let refresh_hits = Arc::new(AtomicUsize::new(0));
    let mock = EphemeralServer::start(
        Router::new()
            .route(
                "/test/google-tokeninfo",
                post({
                    let tokeninfo_hits = tokeninfo_hits.clone();
                    move |body: String| {
                        let tokeninfo_hits = tokeninfo_hits.clone();
                        async move {
                            tokeninfo_hits.fetch_add(1, Ordering::SeqCst);
                            if body.contains("access_token=native-agy-token") {
                                (
                                    StatusCode::OK,
                                    Json(json!({
                                        "aud": "verify-google-client.apps.googleusercontent.com",
                                        "email": "ag-a@example.test",
                                        "email_verified": "true",
                                        "expires_in": "1800"
                                    })),
                                )
                                    .into_response()
                            } else {
                                (StatusCode::BAD_REQUEST, Json(json!({"error_description": "Invalid Value"}))).into_response()
                            }
                        }
                    }
                }),
            )
            .route(
                "/test/google-token",
                post({
                    let refresh_hits = refresh_hits.clone();
                    move || {
                        let refresh_hits = refresh_hits.clone();
                        async move {
                            refresh_hits.fetch_add(1, Ordering::SeqCst);
                            Json(json!({"access_token": "ag-refreshed-access", "expires_in": 3600}))
                        }
                    }
                }),
            )
            .route(
                "/v1internal:generateContent",
                post(|headers: HeaderMap| async move {
                    if router::bearer(&headers) == Some("ag-stale-access") {
                        (StatusCode::UNAUTHORIZED, Json(json!({"error": {"code": 401, "message": "expired", "status": "UNAUTHENTICATED"}})))
                            .into_response()
                    } else if router::bearer(&headers) == Some("ag-refreshed-access") {
                        (StatusCode::OK, Json(json!({"response": {"candidates": []}}))).into_response()
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": {"code": 401, "message": "unexpected token", "status": "UNAUTHENTICATED"}})),
                        )
                            .into_response()
                    }
                }),
            ),
    )
    .await?;

    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[("ag-a.json", google_cred("ag-stale-access", "ag-refresh-a", "ag-a@example.test", "proj-alpha"))],
    )?;
    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;

    // 1. Unknown bearer rejected with Google error shape
    let rejected = http
        .post(server.url("/v1internal:generateContent"))
        .bearer_auth("unknown-bogus-token")
        .json(&json!({"model": "gemini-4-neon-high"}))
        .send()
        .await?;
    ensure!(rejected.status() == StatusCode::UNAUTHORIZED, "expected 401 for unknown token");
    let rejected_body: Value = rejected.json().await?;
    ensure!(rejected_body["error"]["status"] == "UNAUTHENTICATED", "expected Google UNAUTHENTICATED status on 401");

    // 2. Native agy OAuth token accepted via tokeninfo + upstream 401 triggers forced refresh + retry -> 200
    let ok1 = http
        .post(server.url("/v1internal:generateContent"))
        .bearer_auth("native-agy-token")
        .json(&json!({"model": "gemini-4-neon-high"}))
        .send()
        .await?;
    ensure!(ok1.status() == StatusCode::OK, "expected 200 after 401 refresh recovery, got {}", ok1.status());
    ensure!(refresh_hits.load(Ordering::SeqCst) == 1, "expected exactly 1 OAuth refresh");

    // 3. Second call with same native agy token hits tokeninfo cache (no extra tokeninfo call)
    let before_hits = tokeninfo_hits.load(Ordering::SeqCst);
    let ok2 = http
        .post(server.url("/v1internal:generateContent"))
        .bearer_auth("native-agy-token")
        .json(&json!({"model": "gemini-4-neon-high"}))
        .send()
        .await?;
    ensure!(ok2.status() == StatusCode::OK);
    ensure!(tokeninfo_hits.load(Ordering::SeqCst) == before_hits, "expected tokeninfo cache hit on second call");

    Ok("native agy bearer auth, tokeninfo caching, Google UNAUTHENTICATED 401 shape, and forced OAuth refresh recovery verified".into())
}

async fn check_hub_activity_cache_hit_sync() -> Result<String> {
    let dir = TempDir::new()?;
    let mock = EphemeralServer::start(Router::new().route(
        "/v1internal:streamGenerateContent",
        post(|| async {
            let sse = concat!(
                "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"cached\"}]}}],",
                "\"usageMetadata\":{\"promptTokenCount\":20000,\"candidatesTokenCount\":250,\"thoughtsTokenCount\":80,",
                "\"cacheCreationTokenCount\":1000,\"cacheTokensDetails\":[{\"modality\":\"TEXT\",\"tokenCount\":15000}]}}}\n\n"
            );
            (StatusCode::OK, [("content-type", "text/event-stream")], sse.to_owned())
        }),
    ))
    .await?;

    let proxy = make_proxy(
        &dir,
        &mock.base_url()?,
        &[("ag-a.json", google_cred("ag-access-a", "ag-refresh-a", "ag-a@example.test", "proj-alpha"))],
    )?;
    let server = EphemeralServer::start(proxy.clone().app()).await?;
    let http = http_client()?;

    let res = http
        .post(server.url("/v1internal:streamGenerateContent?alt=sse"))
        .bearer_auth("verify-client-key")
        .json(&json!({
            "model": "gemini-4-neon-high",
            "project": "client-default",
            "request": {"contents": [{"role": "user", "parts": [{"text": "Verify cacheTokensDetails extraction"}]}]}
        }))
        .send()
        .await?;
    ensure!(res.status() == StatusCode::OK);
    let _ = res.text().await?;
    tokio::time::sleep(Duration::from_millis(25)).await;

    let state = proxy.state.read().await;
    let row = state.activity.back().ok_or_else(|| anyhow!("missing recorded activity exchange"))?;
    ensure!(row.session_id.is_none(), "expected no trace session without an explicit ID, got {:?}", row.session_id);
    ensure!(row.input_tokens == 20_000, "expected input_tokens=20000, got {}", row.input_tokens);
    ensure!(row.cache_read_tokens == 15_000, "expected cache_read_tokens=15000 from cacheTokensDetails, got {}", row.cache_read_tokens);
    ensure!(row.cache_write_tokens == 1_000, "expected cache_write_tokens=1000, got {}", row.cache_write_tokens);
    ensure!(row.thinking_tokens == 80, "expected thinking_tokens=80, got {}", row.thinking_tokens);
    let hit_rate = row.cache_read_tokens as f64 / row.input_tokens as f64;
    ensure!((hit_rate - 0.75).abs() < 1e-6, "expected 75% cache hit rate, got {hit_rate}");

    Ok("Hub activity preserves cache-read (15,000/20,000), cache-write (1,000) and thinking tokens (80) without a derived session ID"
        .into())
}
