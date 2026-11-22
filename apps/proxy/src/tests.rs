use super::*;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::TimeDelta;
use contracts::providers::RouteStrategy::*;
use serde_json::json;
use sha2::Digest;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use tokio::{net::TcpListener, task::JoinHandle};

#[test]
fn routing_strategies_and_session_tree() {
    let mut pool = pool::Pool::default();
    for (name, priority, weight) in [("a.json", 0, 2), ("b.json", 1, 1)] {
        pool.insert(
            pool::parse_credentials(
                name,
                &json!({
                    "type": "claude", "access_token": format!("token-{name}"), "priority": priority, "weight": weight
                }),
                false,
            )
            .unwrap(),
        );
    }
    for (strategy, expected) in [
        (SoonestReset, ["b.json", "b.json", "b.json"]),
        (FillFirst, ["b.json", "b.json", "b.json"]),
        (RoundRobin, ["a.json", "b.json", "a.json"]),
        (WeightedRoundRobin, ["a.json", "a.json", "b.json"]),
    ] {
        pool.policy.strategy = strategy;
        pool.cursors.clear();
        for id in expected {
            assert_eq!(pool.pick(pool::Lane::Claude, None, None, "", &[]).unwrap().id, id);
        }
    }
    pool.bind(pool::Lane::Claude, "root", "a.json");
    pool.bind(pool::Lane::Claude, "child", "a.json");
    pool.link(pool::Lane::Claude, "child", "root");
    assert_eq!(pool.pick(Lane::Claude, Some("root"), None, "", &[]).unwrap().id, "a.json");
    assert_eq!(pool.move_session("root", "b.json", false).unwrap(), 1);
    assert_eq!(pool.bound(Lane::Claude, "child"), Some("a.json"));
    assert_eq!(pool.move_session("root", "b.json", true).unwrap(), 2);
    assert_eq!(pool.pick(pool::Lane::Claude, Some("child"), None, "", &[]).unwrap().id, "b.json");
    let now = Utc::now();
    for (id, hours) in [("a.json", 2), ("b.json", 5)] {
        let raw = json!({"seven_day":{"utilization": 40, "resets_at": now + TimeDelta::hours(hours)}});
        pool.accounts.get_mut(id).unwrap().windows = integrations::providers::claude::parse_claude_oauth_usage(&raw, now);
    }
    pool.policy.strategy = SoonestReset;
    assert_eq!(pool.pick(Lane::Claude, None, None, "", &[]).unwrap().id, "a.json");
    pool.insert(pool::parse_credentials("key.json", &json!({"type":"api_key","family":"claude","api_key":"test-key"}), false).unwrap());
    pool.policy.api_key_order = contracts::providers::ApiKeyOrder::KeysFirst;
    assert_eq!(pool.pick(Lane::Claude, Some("root"), None, "", &[]).unwrap().id, "key.json");
    pool.policy.api_key_order = contracts::providers::ApiKeyOrder::SubscriptionsOnly;
    assert_eq!(pool.pick(Lane::Claude, Some("root"), None, "", &[]).unwrap().id, "b.json");
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("proxy-test-{}", Uuid::new_v4()));
        config::private_dir(&path).unwrap();
        Self(path)
    }
    fn join(&self, path: &str) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn registration_and_private_keys() {
    let dir = Scratch::new();
    let path = dir.join("userdata/settings.json");
    config::write_private(&path, br#"{"theme":"dark","usageLimitSources":{"other":{"enabled":false}}}"#, true).unwrap();
    let config = Config::new(config::DEFAULT_BIND.parse().unwrap(), &dir.join("state"), None, false, None, None).unwrap();
    assert_ne!(config.client_key, config.management_key);
    for name in ["client-key", "management-key"] {
        let path = dir.join(&format!("state/{name}"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(path).unwrap().len(), 64);
    }
    for label in ["Old label", "New label", "New label"] {
        clients::register_t3code(&dir.0, "proxy", label, config::DEFAULT_URL, "management-fixture").unwrap();
    }
    let settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        settings,
        json!({"theme":"dark","usageLimitSources":{
            "other":{"enabled":false},
            "proxy":{"kind":"cliproxy","label":"New label","url":config::DEFAULT_URL,"managementKey":"management-fixture","enabled":true}
        }})
    );
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn client_wiring_is_reversible_without_discarding_later_edits() {
    use clients::Client::*;
    let dir = Scratch::new();
    let claude = dir.join("claude/settings.json");
    let prior = br#"{"theme":"light","env":{"ANTHROPIC_AUTH_TOKEN":"old","ANTHROPIC_CUSTOM_HEADERS":"X-Trace: on\nx-api-key: old"}}"#;
    config::write_private(&claude, prior, true).unwrap();
    let url = config::DEFAULT_URL;
    ClaudeCode.wire_at(&claude, Some((url, "one"))).unwrap();
    ClaudeCode.wire_at(&claude, Some((url, "two"))).unwrap();
    let mut v: Value = serde_json::from_slice(&fs::read(&claude).unwrap()).unwrap();
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], url);
    assert_eq!(v["env"]["ANTHROPIC_CUSTOM_HEADERS"], "X-Trace: on\nX-Api-Key: two");
    assert!(!v["env"].as_object().unwrap().contains_key("ANTHROPIC_AUTH_TOKEN"));
    let backup = dir.join("claude/settings.json.spillway-backup");
    assert_eq!(fs::read(&backup).unwrap(), prior);
    v["theme"] = json!("dark");
    v["env"]["UNRELATED"] = json!("changed");
    fs::write(&claude, serde_json::to_vec(&v).unwrap()).unwrap();
    ClaudeCode.wire_at(&claude, None).unwrap();
    let v: Value = serde_json::from_slice(&fs::read(&claude).unwrap()).unwrap();
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["env"]["UNRELATED"], "changed");
    assert_eq!(v["env"]["ANTHROPIC_AUTH_TOKEN"], "old");
    assert_eq!(v["env"]["ANTHROPIC_CUSTOM_HEADERS"], "X-Trace: on\nx-api-key: old");
    assert!(!backup.exists());

    let codex = dir.join("codex/config.toml");
    let auth = dir.join("codex/auth.json");
    let original =
        "model_provider = 'other'\nopenai_base_url = \"https://example.test\"\n[model_providers.other]\nmodel_provider = \"untouched\"\n";
    config::write_private(&codex, original.as_bytes(), true).unwrap();
    config::write_private(&auth, br#"{"tokens":{"refresh_token":"spillway-pool"}}"#, true).unwrap();
    Codex.wire_at(&codex, Some((url, "must-not-appear"))).unwrap();
    assert!(!auth.exists());
    Codex.wire_at(&codex, Some((url, "must-not-appear"))).unwrap();
    let applied = fs::read_to_string(&codex).unwrap();
    assert_eq!(applied.matches("# >>> spillway").count(), 1);
    assert!(applied.contains(&format!("openai_base_url = \"{url}/backend-api/codex\"")));
    assert!(applied.contains("# spillway disabled: model_provider = 'other'"));
    assert!(!applied.contains("must-not-appear"));
    let backup = dir.join("codex/config.toml.spillway-backup");
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    let extra = "new_setting = true\n";
    fs::write(&codex, format!("{applied}{extra}")).unwrap();
    fs::write(&auth, br#"{"tokens":{"refresh_token":"native-login"}}"#).unwrap();
    Codex.wire_at(&codex, None).unwrap();
    assert_eq!(fs::read_to_string(&codex).unwrap(), format!("{original}{extra}"));
    assert!(!backup.exists());
    assert!(auth.exists());
}

struct Server(reqwest::Url, JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.1.abort();
    }
}

async fn serve(app: Router) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    Server(url, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
}

fn fixture(dir: &Scratch, mock: &Server) -> Arc<Proxy> {
    let auth_dir = dir.join("auth");
    for (name, priority, access) in [("a.json", 1, "upstream-fixture-a"), ("b.json", 0, "upstream-fixture-b")] {
        config::write_private(
            &auth_dir.join(name),
            &serde_json::to_vec(&json!({
                "type":"claude","access_token":access,"priority":priority,"email":"test@example.test","models":["claude-fixture"]
            }))
            .unwrap(),
            true,
        )
        .unwrap();
    }
    let config = Config {
        bind: config::DEFAULT_BIND.parse().unwrap(),
        home: dir.0.clone(),
        t3code_home: dir.0.clone(),
        auth_dir,
        discover_local: false,
        provenance_oauth_dir: dir.join("provenance-oauth"),
        provenance_oauth_bin: None,
        client_key: "inference-fixture".into(),
        management_key: "management-fixture".into(),
        anthropic: mock.0.clone(),
        openai: mock.0.clone(),
        codex: mock.0.clone(),
        antigravity: mock.0.clone(),
        url: config::DEFAULT_URL.into(),
    };
    let mut proxy = Proxy::new(config).unwrap();
    proxy.management_mock = Some(mock.0.clone());
    Arc::new(proxy)
}

struct Http<'a>(&'a Server, reqwest::Client);

impl<'a> Http<'a> {
    fn new(server: &'a Server) -> Self {
        Self(server, reqwest::Client::builder().no_proxy().build().unwrap())
    }
    fn request(&self, method: &str, path: &str) -> reqwest::RequestBuilder {
        self.1.request(method.parse().unwrap(), self.0.0.join(path).unwrap())
    }
    async fn management(&self, method: &str, path: &str, body: Option<Value>) -> reqwest::Response {
        let req = self.request(method, path).bearer_auth("management-fixture");
        if let Some(body) = body { req.json(&body) } else { req }.send().await.unwrap()
    }
}

async fn anthropic_mock(headers: HeaderMap) -> axum::response::Response {
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert!(headers["anthropic-beta"].to_str().unwrap().contains("oauth-2025-04-20"));
    assert!(!headers.contains_key("x-api-key"));
    match headers["authorization"].to_str().unwrap() {
        "Bearer upstream-fixture-a" => (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], "limited").into_response(),
        "Bearer upstream-fixture-b" => {
            let chunks = [
                "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":7,\"cache_read_input_tokens\":",
                "3,\"cache_creation_input_tokens\":2}}}\n\nevent: message_delta\ndata: {\"usage\":{\"output_tokens\":4},\"content\":\"upstream-fixture-b\"}\n\n",
            ];
            let body = Body::from_stream(futures_util::stream::iter(
                chunks.map(|text| Ok::<_, std::io::Error>(Bytes::from_static(text.as_bytes()))),
            ));
            (StatusCode::OK, [("content-type", "text/event-stream"), ("anthropic-ratelimit-tokens-remaining", "123")], body).into_response()
        }
        _ => panic!("unexpected routed OAuth token"),
    }
}

#[tokio::test]
async fn cooldown_failover_stream_usage_and_capture() {
    let mock = serve(Router::new().route("/v1/messages", post(anthropic_mock))).await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    assert_eq!(
        http.management("PUT", "/v8/management/observability/captures/lease", Some(json!({"enabled":true,"version":2}))).await.status(),
        200
    );
    let send = || {
        http.request("POST", "/v1/messages")
            .header("x-api-key", "inference-fixture")
            .header("anthropic-version", "2023-06-01")
            .header("x-session-id", "session-1")
            .json(&json!({"model":"claude-fixture","messages":[{"role":"user","content":"management-fixture"}],"stream":true}))
    };
    for _ in 0..2 {
        let response = send().send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["anthropic-ratelimit-tokens-remaining"], "123");
        let text = response.text().await.unwrap();
        assert!(text.contains("event: message_start"));
        assert!(text.contains("event: message_delta"));
        assert!(text.ends_with("\n\n"));
    }
    let state = proxy.state.read().await;
    assert!(state.pool.accounts["a.json"].cooldown.is_some_and(|t| t > Utc::now() + TimeDelta::seconds(50)));
    assert_eq!(state.pool.bound(Lane::Claude, "session-1"), Some("b.json"));
    assert_eq!(state.activity.len(), 2);
    assert_eq!(state.activity[0].attempts.iter().map(|a| a.status_code).collect::<Vec<_>>(), [Some(429), Some(200)]);
    assert_eq!(state.activity[1].attempts.len(), 1);
    let activity = &state.activity[0];
    let stored = activity.attempts[1].captured_body.as_ref().unwrap().to_string();
    assert!(stored.contains("[REDACTED]"));
    assert!(!stored.contains("fixture"));
    assert_eq!((activity.input_tokens, activity.output_tokens, activity.cache_read_tokens, activity.cache_write_tokens), (12, 4, 3, 2));
    drop(state);
    let page: Value = http.management("GET", "/v8/management/observability/captures?after=0", None).await.json().await.unwrap();
    assert_eq!(page["version"], 2);
    let records = page["records"].as_array().unwrap();
    assert_eq!(records.iter().map(|r| r["event"].as_str().unwrap()).collect::<Vec<_>>(), ["start", "data", "data", "end"]);
    for (direction, record) in [("request", &records[1]), ("response", &records[2])] {
        assert_eq!(record["chunk"]["direction"], direction);
        let data = String::from_utf8(STANDARD.decode(record["chunk"]["data"].as_str().unwrap()).unwrap()).unwrap();
        assert!(data.contains("[REDACTED]"));
        assert!(!data.contains("fixture"));
        assert_eq!(records[3][format!("{direction}Sha256")], hex::encode(sha2::Sha256::digest(data.as_bytes())));
    }
    assert_eq!(
        http.management("POST", "/v8/management/observability/captures/ack", Some(json!({"epoch":page["epoch"],"sequence":4})))
            .await
            .status(),
        200
    );
    assert_eq!(http.management("DELETE", "/v8/management/observability/captures", None).await.status(), 200);
    let page: Value = http.management("GET", "/v8/management/observability/captures", None).await.json().await.unwrap();
    assert_eq!(page["records"], json!([]));
    proxy.state.write().await.pool.accounts.get_mut("b.json").unwrap().cooldown = Some(Utc::now() + TimeDelta::seconds(30));
    let limited = send().send().await.unwrap();
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.headers()["anthropic-ratelimit-unified-status"], "rejected");
}

#[tokio::test]
async fn management_wire_contracts_reject_cross_origin_and_manage_pool() {
    let mock = serve(Router::new().route(
        "/api/oauth/usage",
        get(|headers: HeaderMap| async move {
            assert_eq!(headers["authorization"], "Bearer upstream-fixture-a");
            assert_eq!(headers["anthropic-beta"], "oauth-2025-04-20");
            axum::Json(json!({"five_hour":{"utilization":42,"resets_at":"2026-11-22T04:00:00Z"}}))
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let path = "/v0/management/auth-files";
    assert_eq!(http.request("GET", path).bearer_auth("inference-fixture").send().await.unwrap().status(), 401);
    let body = http.management("GET", path, None).await.text().await.unwrap();
    assert!(!body.contains("upstream-fixture"));
    let inventory: Value = serde_json::from_str(&body).unwrap();
    let row = &inventory["files"][0];
    let index = row["auth_index"].as_str().unwrap();
    assert_eq!(row["provider"], "claude");
    assert_eq!(row["email"], "test@example.test");
    let request = |url: &str| {
        json!({
            "auth_index":index,"method":"GET","url":url,
            "header":{"Authorization":"Bearer $TOKEN$","anthropic-beta":"oauth-2025-04-20"}
        })
    };
    let call = "/v0/management/api-call";
    let reply: Value =
        http.management("POST", call, Some(request(mock.0.join("api/oauth/usage").unwrap().as_str()))).await.json().await.unwrap();
    assert_eq!(reply["status_code"], 200);
    assert!(reply["body"].as_str().unwrap().contains("utilization"));
    for url in [
        "http://169.254.169.254/latest/meta-data",
        "https://api.anthropic.com@example.test/api/oauth/usage",
        "https://chatgpt.com/backend-api/wham/usage",
        "https://api.anthropic.com/redirect",
    ] {
        assert_eq!(http.management("POST", call, Some(request(url))).await.status(), 403);
    }
    let refresh: Value =
        http.management("POST", "/v8/management/usage/refresh", Some(json!({"auth_index":index}))).await.json().await.unwrap();
    assert_eq!(refresh["errors"], json!({}));
    assert_eq!(refresh["credentials"][0]["windows"][0]["duration_seconds"], 18000);
    assert_eq!(refresh["credentials"][0]["windows"][0]["used_percent"], 42.0);
    proxy.state.write().await.pool.accounts.get_mut("a.json").unwrap().cooldown = Some(Utc::now() + TimeDelta::seconds(120));
    let reset: Value =
        http.management("POST", "/v0/management/reset-quota", Some(json!({"auth_index": index}))).await.json().await.unwrap();
    assert_eq!(reset["windows_reset"], 1);
    assert!(proxy.state.read().await.pool.accounts["a.json"].cooldown.is_none());
    let strategy = "/v8/management/config/routing/strategy";
    let reply = http.management("GET", strategy, None).await;
    for name in ["x-cpa-version", "x-cpa-commit", "x-cpa-build-date"] {
        assert!(reply.headers().contains_key(name));
    }
    assert_eq!(http.management("PUT", strategy, Some(json!("weighted-round-robin"))).await.status(), 200);
    let policy: Value = http.management("GET", "/v8/management/routing/policy", None).await.json().await.unwrap();
    assert_eq!(policy["strategy"], "weighted_round_robin");
    assert_eq!(
        http.management("PUT", "/v8/management/routing/preferred-auth", Some(json!({"auth_index":index,"provider":"claude"})))
            .await
            .status(),
        200
    );
    let models: Value = http.management("GET", "/v8/management/credentials/models?name=a.json", None).await.json().await.unwrap();
    assert_eq!(models["models"], json!([{"id":"claude-fixture"}]));
    let credentials = "/v8/management/credentials";
    let upload = "/v8/management/credentials?name=codex.json";
    assert_eq!(
        http.management("POST", upload, Some(json!({"type":"codex","access_token":"access-fixture","account_id":"account-123"})))
            .await
            .status(),
        200
    );
    assert_eq!(
        http.management("PATCH", &format!("{credentials}/status"), Some(json!({"name":"codex.json","disabled":true}))).await.status(),
        200
    );
    assert_eq!(
        http.management(
            "PATCH",
            &format!("{credentials}/fields"),
            Some(json!({"name":"codex.json","weight":3,"priority":5,"note":"team"}))
        )
        .await
        .status(),
        200
    );
    let inventory: Value = http.management("GET", credentials, None).await.json().await.unwrap();
    let codex = &inventory["files"][2];
    assert_eq!(codex["id_token"]["chatgpt_account_id"], "account-123");
    assert_eq!(codex["disabled"], true);
    assert_eq!(codex["weight"], 3);
    assert_eq!(codex["priority"], 5);
    assert_eq!(codex["note"], "team");
    assert_eq!(http.management("DELETE", upload, None).await.status(), 200);
    let logs: Value = http.management("GET", "/v8/management/observability/logs", None).await.json().await.unwrap();
    assert!(logs["lines"].is_array());
    assert_eq!(http.management("POST", "/v8/management/server/shutdown", None).await.status(), 202);
    proxy.shutdown_requested().await;
}

#[tokio::test]
async fn google_placeholder_refreshes_and_caches_oauth() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let refreshes = Arc::new(AtomicUsize::new(0));
    let tokens = refreshes.clone();
    let mock = serve(
        Router::new()
            .route(
                "/test/google-token",
                post(move |body: String| {
                    let hits = tokens.clone();
                    async move {
                        assert!(body.contains("grant_type=refresh_token"));
                        assert!(body.contains("refresh_token=google-refresh-fixture"));
                        hits.fetch_add(1, Ordering::Relaxed);
                        axum::Json(json!({"access_token":"google-access-fixture","expires_in":3600}))
                    }
                }),
            )
            .route(
                "/v1internal:fetchAvailableModels",
                post(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer google-access-fixture");
                    axum::Json(json!({"models":{"gemini-test":{"displayName":"Gemini fixture"}}}))
                }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let credentials = pool::parse_credentials(
        "google.json",
        &json!({
            "type":"antigravity","refresh_token":"google-refresh-fixture","client_id":"fixture-client","client_secret":"fixture-secret"
        }),
        true,
    )
    .unwrap();
    let index = credentials[0].auth_index.clone();
    proxy.state.write().await.pool.insert(credentials);
    let server = serve(proxy.app()).await;
    let http = Http::new(&server);
    let path = "/v1internal:fetchAvailableModels";
    let reply: Value = http
        .management(
            "POST",
            "/v0/management/api-call",
            Some(json!({
                "auth_index":index,"url":mock.0.join(path).unwrap().as_str(),"method":"POST",
                "header":{"Authorization":"Bearer $TOKEN$"}
            })),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(reply["status_code"], 200);
    let response = http.request("POST", path).bearer_auth("inference-fixture").send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(refreshes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn reset_credit_claims_exact_grant_and_never_substitutes_unknown_scope() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let redeemed = Arc::new(AtomicBool::new(false));
    let reads = redeemed.clone();
    let writes = redeemed.clone();
    let mock = serve(
        Router::new()
            .route(
                "/api/oauth/usage",
                get(move |headers: HeaderMap, uri: axum::http::Uri| {
                    let reads = reads.clone();
                    async move {
                        assert_eq!(uri.query(), Some("cedar_ember=1"));
                        assert_eq!(headers["authorization"], "Bearer reset-fixture");
                        let spent = reads.load(Ordering::Relaxed);
                        axum::Json(json!({
                            "five_hour":{"utilization":if spent {0} else {100},"resets_at":Utc::now() + TimeDelta::hours(1)},
                            "cedar_ember":{"eligible":true,"next_grant_id":"general","grants":[
                                {"id":"general","resets_left":if spent {0} else {1},"usable_now":true,
                                 "clears":["five_hour","seven_day","seven_day_overage_included"],"gives_back_weekly":false},
                                {"id":"other","resets_left":1,"usable_now":true,"clears":["unknown"],"gives_back_weekly":true}
                            ]}
                        }))
                    }
                }),
            )
            .route(
                "/api/organizations/00000000-0000-0000-0000-000000000001/reset_rate_limits",
                post(move |axum::Json(raw): axum::Json<Value>| {
                    let writes = writes.clone();
                    async move {
                        assert_eq!(raw["program"], "cedar_ember");
                        assert_eq!(raw["grant_id"], "general");
                        assert!(Uuid::parse_str(raw["request_id"].as_str().unwrap()).is_ok());
                        assert!(!writes.swap(true, Ordering::Relaxed));
                        axum::Json(json!({"result":"reset"}))
                    }
                }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let credentials = pool::parse_credentials(
        "reset.json",
        &json!({
            "type":"claude","access_token":"reset-fixture", "organization_uuid":"00000000-0000-0000-0000-000000000001"
        }),
        true,
    )
    .unwrap();
    let index = credentials[0].auth_index.clone();
    proxy.state.write().await.pool.insert(credentials);
    let server = serve(proxy.app()).await;
    let http = Http::new(&server);
    for (grant, expected) in [("missing", "no_credit"), ("other", "no_credit"), ("general", "reset")] {
        let reply: Value = http
            .management(
                "POST",
                "/v8/management/usage/reset-credit",
                Some(json!({
                    "auth_index":index,"grant_id":grant
                })),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(reply["code"], expected);
        assert_eq!(redeemed.load(Ordering::Relaxed), expected == "reset");
    }
}

#[tokio::test]
async fn native_codex_data_plane_injects_pooled_identity_and_rejects_unsigned_login() {
    let responses = |headers: HeaderMap| async move {
        assert_eq!(headers["authorization"], "Bearer codex-fixture");
        assert_eq!(headers["chatgpt-account-id"], "pooled-account");
        axum::Json(json!({"usage":{"input_tokens":8,"output_tokens":2}}))
    };
    let mock = serve(Router::new().route("/backend-api/codex/responses", post(responses)).route("/v1/responses", post(responses))).await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    proxy.state.write().await.pool.insert(
        pool::parse_credentials(
            "codex.json",
            &json!({
                "type":"codex","access_token":"codex-fixture","account_id":"pooled-account"
            }),
            true,
        )
        .unwrap(),
    );
    let server = serve(proxy.app()).await;
    let http = Http::new(&server);
    for path in ["/backend-api/codex/responses", "/v1/responses"] {
        assert_eq!(http.request("POST", path).bearer_auth("codex-fixture").send().await.unwrap().status(), 401);
        let response = http
            .request("POST", path)
            .bearer_auth("inference-fixture")
            .header("Chatgpt-Account-Id", "client-spoof")
            .json(&json!({"model":"gpt-fixture"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
}

#[tokio::test]
async fn hub_sync_pushes_observations_activity_and_operation_receipt() {
    use axum::{Json, extract::Path};
    use contracts::providers::{
        AccountCommand, ActivityIngestBatch, ClaimOperationsIn, ObservationBatch, OperationReceiptIn, OperationStatus, ProviderCommand,
        ProviderOperation,
    };
    let (sent, receipt) = tokio::sync::oneshot::channel();
    let sent = Arc::new(Mutex::new(Some(sent)));
    let families = Arc::new(Mutex::new(Vec::new()));
    let observed = families.clone();
    let now = Utc::now();
    let op = ProviderOperation {
        id: Uuid::new_v4(),
        provider_id: "claude".into(),
        actor: "test".into(),
        idempotency_key: "test".into(),
        command_kind: "account.set_priority".into(),
        command: ProviderCommand::Account(AccountCommand::SetPriority { account_id: "claude:a.json".into(), priority: 6 }),
        target_account_id: None,
        target_host: None,
        target_user: None,
        expected_revision: None,
        status: OperationStatus::Dispatched,
        receipt: None,
        error: None,
        created_at: now,
        updated_at: now,
    };
    let claiming = op.clone();
    let mock = serve(Router::new()
        .route("/v1/messages", post(|_: HeaderMap| async { Json(json!({"usage":{"input_tokens":1,"output_tokens":2}})) }))
        .route("/v1/providers/{family}/observations", post(move |Path(family): Path<String>, headers: HeaderMap, Json(batch): Json<ObservationBatch>| {
            let observed = observed.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer hub-fixture");
                assert_eq!(family, batch.provider_id);
                if family == "claude" { assert_eq!(batch.accounts.len(), 2); }
                observed.lock().await.push(family.clone());
                Json(json!({"provider_id":family,"accounts_upserted":batch.accounts.len(),"samples_inserted":0,"grants_upserted":0,"recorded_at":Utc::now()}))
            }
        }))
        .route("/v1/activity/ingest", post(|Json(batch): Json<ActivityIngestBatch>| async move {
            assert_eq!(batch.exchanges.len(), 1);
            assert_eq!(batch.exchanges[0].output_tokens, 2);
            Json(json!({"accepted":1,"deduplicated":0,"recorded_at":Utc::now()}))
        }))
        .route("/v1/providers/operations/claim", post(move |Json(body): Json<ClaimOperationsIn>| {
            let operation = claiming.clone();
            async move {
                assert_eq!(body.provider_ids, ["claude","antigravity","opencode"]);
                Json(vec![operation])
            }
        }))
        .route("/v1/providers/operations/{id}/receipt", post(move |Path(id): Path<Uuid>, Json(body): Json<OperationReceiptIn>| {
            let sent = sent.clone();
            let mut operation = op.clone();
            async move {
                assert_eq!(id, operation.id);
                if let Some(sent) = sent.lock().await.take() { sent.send(body.status).unwrap(); }
                operation.status = body.status;
                Json(operation)
            }
        }))).await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let response = http
        .request("POST", "/v1/messages")
        .bearer_auth("inference-fixture")
        .json(&json!({"model":"claude-fixture"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.bytes().await.unwrap();
    let sync = tokio::spawn(hub_sync::run(proxy.clone(), mock.0.to_string(), "hub-fixture".into()));
    let receipt = tokio::time::timeout(Duration::from_secs(5), receipt).await;
    sync.abort();
    assert_eq!(receipt.unwrap().unwrap(), OperationStatus::Succeeded);
    let mut families = families.lock().await.clone();
    families.sort_unstable();
    assert_eq!(families, ["antigravity", "claude", "opencode"]);
    let state = proxy.state.read().await;
    assert!(state.activity.is_empty());
    assert_eq!(state.pool.accounts["a.json"].priority, 6);
}

#[tokio::test]
async fn rolling_start_targets_one_account_and_does_not_restart_an_open_window() {
    let mock = serve(
        Router::new()
            .route(
                "/v1/messages",
                post(|headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer upstream-fixture-b");
                    assert_eq!(body["max_tokens"], 1);
                    assert_eq!(body["model"], "claude-fixture");
                    axum::Json(json!({"usage":{"input_tokens":4,"output_tokens":1}}))
                }),
            )
            .route(
                "/api/oauth/usage",
                get(|| async { axum::Json(json!({"five_hour":{"utilization":1,"resets_at": Utc::now() + TimeDelta::hours(5)}})) }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let path = "/v8/management/usage/start-window";
    let body = json!({"auth_index":"b.json","model":"claude-fixture"});
    let response = http.management("POST", path, Some(body.clone())).await;
    assert_eq!(response.status(), 200);
    let snapshot: Value = response.json().await.unwrap();
    assert_eq!(snapshot["usage"]["credentials"][1]["windows"][0]["used_percent"], 1.0);
    assert_eq!(http.management("POST", path, Some(body)).await.status(), 409);
    let state = proxy.state.read().await;
    assert_eq!(state.activity.len(), 1);
    assert_eq!(state.activity[0].output_tokens, 1);
    assert_eq!(state.activity[0].purpose, contracts::providers::ActivityPurpose::WindowStarter);
}

#[tokio::test]
async fn multi_key_group_edits_only_affect_the_selected_key_and_hub_ids_are_source_scoped() {
    let dir = Scratch::new();
    let mock = serve(Router::new()).await;
    let proxy = fixture(&dir, &mock);
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);

    // Upload a 2-key API group credential document.
    let upload = http
        .management(
            "POST",
            "/v8/management/credentials?name=group.json",
            Some(json!({
                "type": "api_key",
                "family": "anthropic",
                "keys": ["sk-ant-key-one", "sk-ant-key-two"]
            })),
        )
        .await;
    assert_eq!(upload.status(), 200);

    // Disable and reprioritize ONLY key #1; sibling key #2 must stay enabled with defaults.
    let edit = http
        .management(
            "PATCH",
            "/v8/management/credentials/fields",
            Some(json!({
                "account_id": "group.json#1",
                "disabled": true,
                "priority": 9,
                "weight": 3,
                "note": "first key only"
            })),
        )
        .await;
    assert_eq!(edit.status(), 200);

    let state = proxy.state.read().await;
    let first = &state.pool.accounts["group.json#1"];
    let second = &state.pool.accounts["group.json#2"];
    assert!(first.disabled);
    assert_eq!(first.priority, 9);
    assert_eq!(first.weight, 3);
    assert_eq!(first.note.as_deref(), Some("first key only"));
    assert!(!second.disabled, "sibling key must remain enabled");
    assert_eq!(second.priority, 0, "sibling key priority must be preserved");
    assert_eq!(second.weight, 1, "sibling key weight must be preserved");
    assert_eq!(second.note, None, "sibling key note must be preserved");

    // Verify source-namespaced Hub account IDs distinguish identical local names across hosts/users.
    let id_a = first.hub_id_for("atlas", "can");
    let id_b = first.hub_id_for("nomad", "can");
    assert_ne!(id_a, id_b);
    assert_eq!(id_a, "claude:atlas:can:group.json#1");
    assert_eq!(state.pool.find(&state.pool.hub_id(first)).map(|a| a.id.as_str()), Some("group.json#1"));
}

#[tokio::test]
async fn antigravity_native_auth_project_rewrite_refresh_and_failover() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let tokeninfo_hits = Arc::new(AtomicUsize::new(0));
    let tokeninfo_hits_c = tokeninfo_hits.clone();
    let refresh_hits = Arc::new(AtomicUsize::new(0));
    let refresh_hits_c = refresh_hits.clone();
    let a_attempts = Arc::new(AtomicUsize::new(0));
    let a_attempts_c = a_attempts.clone();

    let mock = serve(
        Router::new()
            .route(
                "/test/google-tokeninfo",
                post(move |body: String| {
                    let hits = tokeninfo_hits_c.clone();
                    async move {
                        hits.fetch_add(1, Ordering::Relaxed);
                        match body.as_str() {
                            "access_token=ya29.client-a" | "access_token=ya29.pool-a-fresh" => axum::Json(json!({
                                "aud": "agy-client-id",
                                "email": "acct-a@example.test",
                                "email_verified": "true",
                                "expires_in": "3599"
                            }))
                            .into_response(),
                            "access_token=ya29.pool-b" => axum::Json(json!({
                                "aud": "agy-client-id",
                                "email": "acct-b@example.test",
                                "email_verified": "true",
                                "expires_in": "3599"
                            }))
                            .into_response(),
                            "access_token=ya29.stranger" => axum::Json(json!({
                                "aud": "agy-client-id",
                                "email": "stranger@example.test",
                                "email_verified": "true",
                                "expires_in": "3599"
                            }))
                            .into_response(),
                            "access_token=ya29.wrong-aud" => axum::Json(json!({
                                "aud": "other-client-id",
                                "email": "acct-a@example.test",
                                "email_verified": "true",
                                "expires_in": "3599"
                            }))
                            .into_response(),
                            "access_token=ya29.unverified" => axum::Json(json!({
                                "aud": "agy-client-id",
                                "email": "acct-a@example.test",
                                "email_verified": "false",
                                "expires_in": "3599"
                            }))
                            .into_response(),
                            _ => StatusCode::BAD_REQUEST.into_response(),
                        }
                    }
                }),
            )
            .route(
                "/test/google-token",
                post(move |body: String| {
                    let hits = refresh_hits_c.clone();
                    async move {
                        hits.fetch_add(1, Ordering::Relaxed);
                        if body.contains("refresh_token=refresh-a") {
                            axum::Json(json!({"access_token":"ya29.pool-a-fresh","expires_in":3600})).into_response()
                        } else if body.contains("refresh_token=refresh-b") {
                            axum::Json(json!({"access_token":"ya29.pool-b","expires_in":3600})).into_response()
                        } else {
                            StatusCode::BAD_REQUEST.into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1internal:streamGenerateContent",
                post(move |headers: HeaderMap, axum::Json(raw): axum::Json<Value>| {
                    let a_attempts = a_attempts_c.clone();
                    async move {
                        match headers["authorization"].to_str().unwrap() {
                            "Bearer ya29.pool-a-stale" => {
                                assert_eq!(raw["project"], "proj-a");
                                StatusCode::UNAUTHORIZED.into_response()
                            }
                            "Bearer ya29.pool-a-fresh" => {
                                assert_eq!(raw["project"], "proj-a");
                                let n = a_attempts.fetch_add(1, Ordering::Relaxed);
                                if n == 0 {
                                    // First fresh call succeeds; second hits 429 and fails over to account B.
                                    let body = "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5,\"cachedContentTokenCount\":3,\"thoughtsTokenCount\":2,\"totalTokenCount\":17}}}\n\n";
                                    (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
                                } else {
                                    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], "rate limited").into_response()
                                }
                            }
                            "Bearer ya29.pool-b" => {
                                assert_eq!(raw["project"], "proj-b");
                                let body = "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":8,\"totalTokenCount\":28}}}\n\n";
                                (StatusCode::OK, [("content-type", "text/event-stream")], body).into_response()
                            }
                            other => panic!("unexpected upstream bearer: {other}"),
                        }
                    }
                }),
            ),
    )
    .await;

    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    // Account A starts with a stale access token (exercises router 401 forced refresh).
    // Account B has lower priority, NO email (exercises lazy email hydration), and a different project_id.
    proxy.state.write().await.pool.insert(
        pool::parse_credentials(
            "google-a.json",
            &json!({
                "type": "antigravity",
                "email": "acct-a@example.test",
                "access_token": "ya29.pool-a-stale",
                "refresh_token": "refresh-a",
                "client_id": "agy-client-id",
                "client_secret": "agy-secret",
                "project_id": "proj-a",
                "priority": 10
            }),
            true,
        )
        .unwrap(),
    );
    proxy.state.write().await.pool.insert(
        pool::parse_credentials(
            "google-b.json",
            &json!({
                "type": "antigravity",
                "refresh_token": "refresh-b",
                "client_id": "agy-client-id",
                "client_secret": "agy-secret",
                "project_id": "proj-b",
                "priority": 1
            }),
            true,
        )
        .unwrap(),
    );

    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let endpoint = "/v1internal:streamGenerateContent?alt=sse";
    let payload = json!({
        "project": "client-default-proj",
        "model": "gemini-4-neon",
        "request": {"session_id": "agy-sess-1"}
    });

    // Reject unauthenticated, unknown-email, wrong-audience, and unverified tokens.
    assert_eq!(http.request("POST", endpoint).json(&payload).send().await.unwrap().status(), 401);
    for bad in ["ya29.stranger", "ya29.wrong-aud", "ya29.unverified"] {
        assert_eq!(http.request("POST", endpoint).bearer_auth(bad).json(&payload).send().await.unwrap().status(), 401);
    }

    // Valid client token for acct-a: hydrates missing emails, authorizes, hits 401 on stale pool-a,
    // force-refreshes pool-a, rewrites project -> proj-a, and records /response/usageMetadata.
    let first = http.request("POST", endpoint).bearer_auth("ya29.client-a").json(&payload).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let _ = first.text().await.unwrap();
    let hits_after_first = tokeninfo_hits.load(Ordering::Relaxed);

    // Second request uses cached client token (zero extra tokeninfo calls), hits 429 on pool-a,
    // and transparently fails over to pool-b with project rewritten -> proj-b!
    let second = http.request("POST", endpoint).bearer_auth("ya29.client-a").json(&payload).send().await.unwrap();
    assert_eq!(second.status(), 200);
    let _ = second.text().await.unwrap();
    assert_eq!(tokeninfo_hits.load(Ordering::Relaxed), hits_after_first, "verified Google token must be cached");

    let state = proxy.state.read().await;
    assert_eq!(state.pool.accounts["google-a.json"].email.as_deref(), Some("acct-a@example.test"));
    assert_eq!(state.pool.accounts["google-b.json"].email.as_deref(), Some("acct-b@example.test"));
    assert_eq!(state.activity.len(), 2);
    assert_eq!(state.activity[0].input_tokens, 12);
    assert_eq!(state.activity[0].output_tokens, 5);
    assert_eq!(state.activity[0].cache_read_tokens, 3);
    assert_eq!(state.activity[0].thinking_tokens, 2);
    assert_eq!(state.activity[1].input_tokens, 20);
    assert_eq!(state.activity[1].output_tokens, 8);
    assert!(refresh_hits.load(Ordering::Relaxed) >= 2);
}

#[tokio::test]
async fn provenance_oauth_discovers_all_antigravity_enrollments_and_deduplicates_acp_token() {
    let dir = Scratch::new();
    let oauth_dir = dir.join("provenance-oauth");
    config::private_dir(&oauth_dir).unwrap();
    let user = hub_sync::source_identity().1;

    // Seed one active ACP profile sharing refresh-1 (opaque hash directory name, as T3 Code writes it).
    let active_hash = "ac0a3dfd6dddb20962cecff6ee5fe65e19d3923be20e52c5ab52ff877f7e4c32";
    let acp_dir = dir.join(&format!("userdata/providers/antigravity/{active_hash}/antigravity-acp"));
    config::write_private(
        &acp_dir.join("acp_token.json"),
        &serde_json::to_vec(&json!({
            "refresh_token": "refresh-1",
            "client_id": "agy-client",
            "client_secret": "agy-secret",
            "project_id": "aicode-consumers"
        }))
        .unwrap(),
        true,
    )
    .unwrap();

    // Create 3 manifests: two google/antigravity enrollments + one unrelated openai manifest.
    for (file, body) in [
        (format!("{user}-google.json"), json!({"target":{"provider":"google","profile":"antigravity","user":user}})),
        (format!("{user}-google-filiz.json"), json!({"target":{"provider":"google","profile":"antigravity","user":user}})),
        (format!("{user}-openai-main.json"), json!({"target":{"provider":"openai","profile":"codex","user":user}})),
    ] {
        config::write_private(&oauth_dir.join(file), &serde_json::to_vec(&body).unwrap(), true).unwrap();
    }

    let fake_bin = dir.join("fake-provenance-oauth");
    fs::write(
        &fake_bin,
        "#!/bin/sh\n\
         case \"$2\" in\n\
           *-google.json) printf '{\"client_id\":\"agy-client\",\"client_secret\":\"agy-secret\",\"project_id\":\"aicode-consumers\",\"refresh_token\":\"refresh-1\"}' ;;\n\
           *-google-filiz.json) printf '{\"client_id\":\"agy-client\",\"client_secret\":\"agy-secret\",\"project_id\":\"aicode-consumers\",\"refresh_token\":\"refresh-2\"}' ;;\n\
           *) exit 1 ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o700)).unwrap();

    let mock = serve(Router::new()).await;
    let config = Config {
        bind: config::DEFAULT_BIND.parse().unwrap(),
        home: dir.0.clone(),
        t3code_home: dir.0.clone(),
        auth_dir: dir.join("auth"),
        discover_local: true,
        provenance_oauth_dir: oauth_dir,
        provenance_oauth_bin: Some(fake_bin),
        client_key: "inference-fixture".into(),
        management_key: "management-fixture".into(),
        anthropic: mock.0.clone(),
        openai: mock.0.clone(),
        codex: mock.0.clone(),
        antigravity: mock.0.clone(),
        url: config::DEFAULT_URL.into(),
    };
    let proxy = Arc::new(Proxy::new(config).unwrap());
    proxy.discover_local_commands().await;

    let state = proxy.state.read().await;
    let mut google: Vec<_> =
        state.pool.accounts.values().filter(|a| a.lane == Lane::Google).map(|a| (a.label.clone(), a.refresh.clone())).collect();
    google.sort();
    assert_eq!(
        google,
        vec![("google".to_owned(), Some("refresh-1".to_owned())), ("google-filiz".to_owned(), Some("refresh-2".to_owned())),]
    );
}
