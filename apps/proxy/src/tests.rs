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
            assert_eq!(pool.pick(Lane::Claude, (None, None), "", None, &[], Utc::now()).unwrap().id, id);
        }
    }
    pool.bind(pool::Lane::Claude, "root", "a.json");
    pool.bind(pool::Lane::Claude, "child", "a.json");
    pool.link(pool::Lane::Claude, "child", "root");
    assert_eq!(pool.pick(Lane::Claude, (Some("root"), None), "", None, &[], Utc::now()).unwrap().id, "a.json");
    assert_eq!(pool.move_session("root", "b.json", false).unwrap(), 1);
    assert_eq!(pool.bound(Lane::Claude, "child"), Some("a.json"));
    assert_eq!(pool.move_session("root", "b.json", true).unwrap(), 2);
    assert_eq!(pool.pick(Lane::Claude, (Some("child"), None), "", None, &[], Utc::now()).unwrap().id, "b.json");
    let now = Utc::now();
    for (id, hours) in [("a.json", 2), ("b.json", 5)] {
        let raw = json!({"seven_day":{"utilization": 40, "resets_at": now + TimeDelta::hours(hours)}});
        pool.accounts.get_mut(id).unwrap().windows = integrations::providers::claude::parse_claude_oauth_usage(&raw, now);
    }
    pool.policy.strategy = SoonestReset;
    assert_eq!(pool.pick(Lane::Claude, (None, None), "", None, &[], now).unwrap().id, "a.json");
    pool.insert(pool::parse_credentials("key.json", &json!({"type":"api_key","family":"claude","api_key":"test-key"}), false).unwrap());
    pool.policy.api_key_order = contracts::providers::ApiKeyOrder::KeysFirst;
    assert_eq!(pool.pick(Lane::Claude, (Some("root"), None), "", None, &[], now).unwrap().id, "key.json");
    pool.policy.api_key_order = contracts::providers::ApiKeyOrder::SubscriptionsOnly;
    assert_eq!(pool.pick(Lane::Claude, (Some("root"), None), "", None, &[], now).unwrap().id, "b.json");
}

#[test]
fn quota_ranking_is_model_specific_and_breaks_weekly_ties_with_session_resets() {
    use integrations::providers::antigravity::{parse_antigravity_models_fallback, parse_antigravity_summary};
    let mut pool = Pool::default();
    let now = Utc::now();
    for (name, weekly, session, neon, shared) in
        [("google.json", 25, 2, 0.0, 0.75), ("google-caniko2.json", 48, 1, 0.8, 0.01), ("google-filiz.json", 30, 3, 0.5, 0.75)]
    {
        let mut accounts = pool::parse_credentials(name, &json!({"type":"google","access_token":name}), false).unwrap();
        accounts[0].windows = parse_antigravity_summary(
            &json!({"buckets":[
                {"bucketId":"gemini-5h", "window":"5h", "remainingFraction":neon, "resetTime":now + TimeDelta::hours(session)},
                {"bucketId":"gemini-weekly", "window":"weekly", "remainingFraction":0.75, "resetTime":now + TimeDelta::hours(weekly)},
                {"bucketId":"gemini-argon-5h", "window":"5h", "remainingFraction":0.995, "resetTime":now + TimeDelta::hours(session)},
                {"bucketId":"gemini-argon-weekly", "window":"weekly", "remainingFraction":0.995, "resetTime":now + TimeDelta::hours(weekly)},
                {"bucketId":"3p-weekly", "window":"weekly", "remainingFraction":shared, "resetTime":now + TimeDelta::hours(20)}
            ]}),
            now,
        );
        pool.insert(accounts);
    }
    const NEON: &str = "gemini-4-neon-high";
    const ARGON: &str = "gemini-4-argon-high";
    let pick = |pool: &mut Pool, model| pool.pick(Lane::Google, (None, None), model, None, &[], now).unwrap().id;
    pool.policy.strategy = SoonestReset;
    assert_eq!(pick(&mut pool, NEON), "google-filiz.json");
    assert_eq!(pick(&mut pool, ARGON), "google.json");
    assert!(pool.accounts["google.json"].eligible(now, ""));
    assert!(pool.accounts["google.json"].quota(&pool)["windows"].as_array().unwrap().iter().all(|w| w["scoped"] == true));
    pool.policy.strategy = FillFirst;
    assert_eq!(pick(&mut pool, NEON), "google-filiz.json");

    pool.policy.strategy = SoonestReset;
    let weekly = |pool: &mut Pool, name: &str, resets_at, used_percent| {
        let w = pool.accounts.get_mut(name).unwrap().windows.iter_mut().find(|w| w.window_key == "gemini-weekly").unwrap();
        w.resets_at = resets_at;
        w.used_percent = used_percent;
    };
    weekly(&mut pool, "google-caniko2.json", Some(now + TimeDelta::hours(30)), Some(25.0));
    assert_eq!(pick(&mut pool, NEON), "google-caniko2.json");
    for name in ["google-caniko2.json", "google-filiz.json"] {
        weekly(&mut pool, name, None, Some(25.0));
    }
    assert_eq!(pick(&mut pool, NEON), "google-caniko2.json");
    weekly(&mut pool, "google-caniko2.json", Some(now + TimeDelta::hours(30)), Some(100.0));
    assert_eq!(pick(&mut pool, NEON), "google-filiz.json");

    let fallback = json!({"models":{
        (NEON): {"quotaInfo":{"resetTime": now + TimeDelta::hours(1)}},
        (ARGON): {"quotaInfo":{"remainingFraction":0.995}}
    }});
    let filiz = pool.accounts.get_mut("google-filiz.json").unwrap();
    filiz.windows = parse_antigravity_models_fallback(&fallback, now);
    assert!(!filiz.eligible(now, NEON));
    assert!(filiz.eligible(now, ARGON));
}

#[test]
fn round_robin_cursors_follow_eligible_accounts() {
    for strategy in [RoundRobin, WeightedRoundRobin] {
        for (lane, first, second) in [(Lane::Google, "gemini-neon", "gemini-argon"), (Lane::Claude, "claude-opus-4", "claude-sonnet-4")] {
            let mut pool = Pool::default();
            pool.policy.strategy = strategy;
            for (name, models) in [("a.json", vec![first]), ("b.json", vec![first, second]), ("c.json", vec![second])] {
                pool.insert(
                    pool::parse_credentials(name, &json!({"type":lane.name(),"access_token":name,"models":models}), false).unwrap(),
                );
            }
            let picks = [first, second, first, second].map(|model| pool.pick(lane, (None, None), model, None, &[], Utc::now()).unwrap().id);
            assert_eq!(picks, ["a.json", "b.json", "b.json", "c.json"]);
        }
    }
}

#[test]
fn distinct_eligible_sets_keep_round_robin_cursors_bounded() {
    let mut pool = Pool::default();
    pool.policy.strategy = RoundRobin;
    for n in 0..260 {
        let name = format!("account-{n}.json");
        let model = format!("model-{n}");
        pool.insert(pool::parse_credentials(&name, &json!({"type":"claude","access_token":name,"models":[model]}), false).unwrap());
        assert_eq!(pool.pick(Lane::Claude, (None, None), &model, None, &[], Utc::now()).unwrap().id, name);
    }
    assert_eq!(pool.cursors.len(), 256);
}

#[test]
fn credentials_require_a_real_match_and_agree_on_refresh_tokens() {
    let account = |refresh: &str| {
        pool::parse_credentials(
            "google.json",
            &json!({"type":"google","refresh_token":refresh,"client_id":"client","client_secret":"secret"}),
            false,
        )
        .unwrap()
        .remove(0)
    };
    let mut a = account("refresh-a");
    let mut b = account("refresh-b");
    assert!(!a.same_credential(&b));
    a.access = "same-access".into();
    b.access.clone_from(&a.access);
    assert!(!a.same_credential(&b));
    let mut renewed = a.clone();
    renewed.access = "new-access".into();
    assert!(a.same_credential(&renewed));
    a.refresh = None;
    assert!(a.same_credential(&b));
    a.access.clear();
    a.refresh = None;
    assert!(!a.same_credential(&a));
}

#[test]
fn quota_matching_uses_bucket_keys_before_display_names() {
    use contracts::providers::{QuotaUnit, QuotaWindowKind};
    let mut w = integrations::providers::claude::parse_claude_oauth_usage(&json!({"seven_day_opus":{"utilization":100}}), Utc::now())
        .into_iter()
        .find(|w| w.window_key == "seven_day_opus")
        .unwrap();
    let applies = pool::window_applies_to_model;
    w.scope_model = Some("Claude Opus".into());
    assert!(applies(&w, "CLAUDE-OPUS-4"));
    assert!(!applies(&w, "claude-sonnet-4"));
    w.window_key = "seven_day_cowork".into();
    assert!(!applies(&w, "claude-opus-4"));
    w.window_key = "gemini-5h".into();
    w.scope_model = Some("Gemini 4 Argon".into());
    assert!(applies(&w, "gemini-4-neon"));
    assert!(!applies(&w, "gemini-4-argon"));
    w.window_key = "3p-weekly".into();
    assert!(applies(&w, "claude-opus-4") && applies(&w, "gpt-5") && !applies(&w, ""));
    w.scope_model = None;
    assert!(applies(&w, ""));
    w.unit = QuotaUnit::Credits;
    assert!(!applies(&w, ""));
    w.unit = QuotaUnit::Percent;
    w.kind = QuotaWindowKind::Spend;
    assert!(!applies(&w, "gpt-5"));
}

pub(super) struct Scratch(PathBuf);
impl Scratch {
    pub(super) fn new() -> Self {
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

pub(super) struct Server(reqwest::Url, JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.1.abort();
    }
}

pub(super) async fn serve(app: Router) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    Server(url, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
}

pub(super) fn fixture(dir: &Scratch, mock: &Server) -> Arc<Proxy> {
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
    let mut proxy = Proxy::new(fixture_config(dir, mock)).unwrap();
    proxy.management_mock = Some(mock.0.clone());
    Arc::new(proxy)
}

fn fixture_config(dir: &Scratch, mock: &Server) -> Config {
    Config {
        bind: config::DEFAULT_BIND.parse().unwrap(),
        home: dir.0.clone(),
        t3code_home: dir.0.clone(),
        auth_dir: dir.join("auth"),
        discover_local: false,
        exclude_local_lanes: Default::default(),
        provenance_oauth_dir: dir.join("provenance-oauth"),
        provenance_oauth_bin: None,
        client_key: "inference-fixture".into(),
        management_key: "management-fixture".into(),
        anthropic: mock.0.clone(),
        openai: mock.0.clone(),
        codex: mock.0.clone(),
        antigravity: mock.0.clone(),
        url: config::DEFAULT_URL.into(),
    }
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

#[test]
fn codex_recovery_clears_synthetic_denial_and_google_merge_prefers_active_over_expired() {
    let now = Utc::now();
    let mut codex_missing_primary = HeaderMap::new();
    codex_missing_primary.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    codex_missing_primary.insert("x-codex-secondary-used-percent", "10".parse().unwrap());
    codex_missing_primary.insert("x-codex-secondary-reset-at", (now + TimeDelta::days(6)).timestamp().to_string().parse().unwrap());
    assert_eq!(router::header_cooldown(Lane::Codex, &codex_missing_primary, now), None);

    let mut codex = pool::parse_credentials("codex.json", &json!({"type":"codex","access_token":"tok"}), false).unwrap().pop().unwrap();
    let (_, denied, _) = integrations::providers::opencode::parse_chatgpt_wham_usage(&json!({"rate_limit":{"allowed":false}}), now);
    assert_eq!(denied[0].window_key, "rate_limit_denied");
    merge_probe_windows(&mut codex, denied, false, now);
    assert!(!codex.eligible(now, "gpt-5"));
    let (_, recovered, _) = integrations::providers::opencode::parse_chatgpt_wham_usage(
        &json!({"rate_limit":{"allowed":true,"secondary_window":{"used_percent":12,"reset_at":(now + TimeDelta::days(3)).timestamp()}}}),
        now,
    );
    merge_probe_windows(&mut codex, recovered, false, now);
    assert!(codex.eligible(now, "gpt-5"));

    let mut expired = integrations::providers::antigravity::parse_antigravity_summary(
        &json!({"buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0,"resetTime":now - TimeDelta::hours(2)}]}),
        now,
    );
    let active_undated = integrations::providers::antigravity::parse_antigravity_summary(
        &json!({"buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0}]}),
        now,
    );
    expired.extend(active_undated);
    let merged = integrations::providers::antigravity::merge_antigravity_windows(expired);
    assert_eq!(merged.len(), 1);
    assert!(merged[0].resets_at.is_none());
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
    assert!(state.pool.accounts["a.json"].cooldown_until().is_some_and(|t| t > Utc::now() + TimeDelta::seconds(50)));
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
    proxy.state.write().await.pool.accounts.get_mut("b.json").unwrap().cooldown =
        Some(pool::Cooldown { until: Utc::now() + TimeDelta::seconds(30), verified_reset: false });
    let limited = send().send().await.unwrap();
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.headers()["anthropic-ratelimit-unified-status"], "rejected");
    assert_eq!(limited.headers()["x-should-retry"], "false");
    assert!(!limited.headers().contains_key("x-fleetix-pool-quota"));
    assert_eq!(
        limited.json::<Value>().await.unwrap(),
        json!({"type":"error","error":{"type":"usage_limit_reached","message":"all eligible accounts are rate limited"}})
    );
}

#[tokio::test]
async fn usage_windows_validates_model_diagnostics_and_does_not_change_the_pool() {
    use contracts::providers::ApiKeyOrder;
    use integrations::providers::antigravity::parse_antigravity_summary;
    use pool::Cooldown;
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mock = serve(Router::new().fallback({
        let probes = probes.clone();
        move || async move {
            probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            StatusCode::NOT_FOUND
        }
    }))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let now = Utc::now();
    let reset = now + TimeDelta::minutes(20);
    let model = "gemini-neon";
    let (accounts, cursors, revision) = {
        let mut state = proxy.state.write().await;
        let pool = &mut state.pool;
        assert!(pool.pick(Lane::Google, (None, None), model, None, &[], now).is_some());
        pool.bind(Lane::Google, "test-session", "google-b.json");
        pool.policy.api_key_order = ApiKeyOrder::SubscriptionsOnly;
        for (name, data) in [
            ("disabled.json", json!({"access_token":"private-token","disabled":true})),
            ("unsupported.json", json!({"access_token":"private-token","models":["different-model"]})),
            ("zero-weight.json", json!({"access_token":"private-token","weight":0})),
            ("key.json", json!({"api_key":"private-token"})),
        ] {
            let mut data = data;
            data["type"] = json!("google");
            pool.insert(pool::parse_credentials(name, &data, false).unwrap());
        }
        let a = pool.accounts.get_mut("google-a.json").unwrap();
        a.scoped_cooldowns.insert("gemini".into(), Cooldown { until: reset, verified_reset: true });
        a.windows =
            parse_antigravity_summary(&json!({"buckets":[{"bucketId":"gemini-weekly","remainingFraction":0.0,"resetTime":reset}]}), now);
        pool.accounts.get_mut("google-b.json").unwrap().auth_cooldown = Some(now + TimeDelta::minutes(2));
        let rows = [
            ("disabled.json", json!(["disabled"])),
            ("google-a.json", json!(["scoped_cooldown", "quota_window"])),
            ("google-b.json", json!(["auth_cooldown"])),
            ("key.json", json!(["subscriptions_only"])),
            ("unsupported.json", json!(["unsupported_model"])),
            ("zero-weight.json", json!(["zero_weight"])),
        ]
        .map(|(id, reasons)| json!({"auth_index":pool.accounts[id].auth_index,"exclusion_reasons":reasons}));
        (rows, pool.cursors.clone(), pool.revision)
    };
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let path = "/v8/management/usage/windows";
    let scoped = format!("{path}?lane=google&model={model}");
    assert_eq!(http.request("GET", &scoped).bearer_auth("inference-fixture").send().await.unwrap().status(), 401);
    assert_eq!(http.request("GET", &format!("{path}?lane=bad")).send().await.unwrap().status(), 401);
    for query in [
        "lane=google",
        "model=gemini-neon",
        "lane=unknown&model=gemini-neon",
        "lane=&model=gemini-neon",
        "lane=google&model=",
        "lane=google&model=%20",
        "lane=google&model=gemini%20neon",
        "lane=google&model=%0A",
        "lane=google&model=%3B",
        "lane=google&model=%C3%A9",
        "lane=google&lane=google&model=gemini-neon",
        "lane=google&model=gemini-neon&unknown=1",
        "unknown=1",
    ] {
        assert_eq!(http.management("GET", &format!("{path}?{query}"), None).await.status(), 400, "{query}");
    }
    let legacy: Value = http.management("GET", path, None).await.json().await.unwrap();
    assert_eq!(legacy.as_object().unwrap().keys().collect::<Vec<_>>(), ["credentials", "hub_sync_configured", "strategy"]);
    assert_eq!(legacy["strategy"], "round-robin");
    assert_eq!(legacy["credentials"].as_array().unwrap().len(), 8);
    assert!(legacy["credentials"].as_array().unwrap().iter().all(|row| row["next_pick"].is_boolean()));

    let response = http.management("GET", &scoped, None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let mut response: Value = response.json().await.unwrap();
    let at: DateTime<Utc> =
        serde_json::from_value(response["model_eligibility"].as_object_mut().unwrap().remove("snapshot_at").unwrap()).unwrap();
    assert!(at >= now && at <= Utc::now());
    assert_eq!(
        response,
        json!({"model_eligibility":{"lane":"antigravity","wire_model":model,"supported_accounts":2,"eligible_accounts":0,
            "verified_next_reset_at":reset,"accounts":accounts}})
    );
    let other: Value = http.management("GET", &format!("{path}?lane=antigravity&model=gemini-argon"), None).await.json().await.unwrap();
    assert_eq!(other["model_eligibility"]["eligible_accounts"], 1);
    assert_eq!(other["model_eligibility"]["verified_next_reset_at"], Value::Null);
    assert_eq!(http.management("GET", path, None).await.json::<Value>().await.unwrap(), legacy);

    let state = proxy.state.read().await;
    assert_eq!(state.pool.cursors, cursors);
    assert_eq!(state.pool.revision, revision);
    assert_eq!(state.pool.bound(Lane::Google, "test-session"), Some("google-b.json"));
    assert_eq!(state.pool.accounts["google-a.json"].scoped_cooldowns["gemini"], Cooldown { until: reset, verified_reset: true });
    assert_eq!(state.pool.accounts["google-b.json"].auth_cooldown, Some(now + TimeDelta::minutes(2)));
    assert!(state.usage_refreshes.is_empty());
    assert!(state.activity.is_empty());
    assert_eq!(probes.load(std::sync::atomic::Ordering::Relaxed), 0);
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
    proxy.state.write().await.pool.accounts.get_mut("a.json").unwrap().cooldown =
        Some(pool::Cooldown { until: Utc::now() + TimeDelta::seconds(120), verified_reset: true });
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
            "type":"antigravity","refresh_token":"google-refresh-fixture","client_id":"fixture-client",
            "client_secret":"fixture-secret","project_id":"fixture-project"
        }),
        true,
    )
    .unwrap();
    let index = credentials[0].auth_index.clone();
    proxy.state.write().await.pool.insert(credentials);
    let server = serve(proxy.app()).await;
    let http = Http::new(&server);
    let inventory = http.management("GET", "/v0/management/auth-files", None).await.text().await.unwrap();
    assert!(!inventory.contains("fixture-secret"));
    let inventory: Value = serde_json::from_str(&inventory).unwrap();
    let google = inventory["files"].as_array().unwrap().iter().find(|row| row["auth_index"] == index).unwrap();
    assert_eq!(google["project_id"], "fixture-project");
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

async fn google_routing_fixture(dir: &Scratch, mock: &Server) -> Arc<Proxy> {
    let proxy = fixture(dir, mock);
    let mut state = proxy.state.write().await;
    state.pool.policy.strategy = RoundRobin;
    for name in ["google-a.json", "google-b.json"] {
        state.pool.insert(
            pool::parse_credentials(name, &json!({"type":"google","access_token":name,"email":"fixture@example.test"}), false).unwrap(),
        );
    }
    drop(state);
    proxy
}

#[tokio::test]
async fn google_rejects_unsupported_encodings_and_unresolved_cache_owners_before_dispatch() {
    let mock = serve(Router::new().route("/v1internal:generateContent", post(|| async { "dispatched" }))).await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let request = || http.request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture");
    for encoding in ["gzip", "br", "deflate", "identity, gzip"] {
        let res = request()
            .header("content-encoding", "identity")
            .header("content-encoding", encoding)
            .json(&json!({"request":{"cachedContent":"projects/alpha/cachedContents/cache"}}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{encoding}");
        assert_eq!(res.headers()["content-type"], "application/json");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], 415);
    }
    for body in [
        json!({"cachedContent":"cachedContents/cache"}),
        json!({"request":{"cachedContent":"cachedContents/cache"}}),
        json!({"request":{"cachedContent":"projects/alpha/cachedContents/cache"},"cachedContent":"cachedContents/cache"}),
        json!({"cachedContent":"projects//cachedContents/cache"}),
    ] {
        let res = request().json(&body).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["status"], "INVALID_ARGUMENT");
    }
    assert!(proxy.state.read().await.activity.is_empty());
    for encoding in ["", "IDENTITY"] {
        let res = request().header("content-encoding", encoding).json(&json!({})).send().await.unwrap();
        assert_eq!(res.text().await.unwrap(), "dispatched");
    }
}

#[tokio::test]
async fn google_cache_aliases_share_project_routing_and_affinity() {
    let mock = serve(
        Router::new()
            .route("/v1internal:generateContent", post(|headers: HeaderMap| async move { router::bearer(&headers).unwrap().to_owned() })),
    )
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    {
        let mut state = proxy.state.write().await;
        state.pool.accounts.get_mut("google-a.json").unwrap().project = Some("alpha".into());
        state.pool.accounts.get_mut("google-b.json").unwrap().project = Some("beta".into());
        state.pool.overrides.insert(Lane::Google, "google-a.json".into());
    }
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let send = |body: Value| http.request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").json(&body);
    let cache = "projects/beta/cachedContents/cache";
    for name in ["cachedContent", "cached_content"] {
        for invalid in [Value::Null, json!(123), json!(""), json!("cachedContents/cache"), json!("projects/beta/")] {
            for body in [json!({(name):invalid}), json!({"request":{(name):invalid}})] {
                let res = send(body.clone()).send().await.unwrap();
                assert_eq!(res.status(), 400, "{body}");
                assert_eq!(res.json::<Value>().await.unwrap()["error"]["status"], "INVALID_ARGUMENT");
            }
        }
    }
    let res = send(json!({"cached_content":cache,"request":{"cachedContent":"projects/alpha/cachedContents/cache"}})).send().await.unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["status"], "INVALID_ARGUMENT");
    assert!(proxy.state.read().await.activity.is_empty());

    for name in ["cached_content", "cachedContent"] {
        for body in [json!({(name):cache}), json!({"request":{(name):cache}})] {
            proxy.state.write().await.pool.clear_affinity();
            assert_eq!(send(body).send().await.unwrap().text().await.unwrap(), "google-b.json");
            let digest = hex::encode(&Sha256::digest(cache.as_bytes())[..12]);
            assert_eq!(proxy.state.read().await.pool.bound(Lane::Google, &format!("#ag-cache-{digest}")), Some("google-b.json"));
        }
    }
}

#[tokio::test]
async fn google_rejects_duplicate_wire_cache_fields_before_dispatch() {
    let mock = serve(Router::new().route("/v1internal:generateContent", post(|| async { "dispatched" }))).await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.accounts.get_mut("google-a.json").unwrap().project = Some("alpha".into());
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    for body in [
        r#"{"request":{"cachedContent":"projects/beta/cachedContents/x"},"request":{"cachedContent":"projects/alpha/cachedContents/x"}}"#,
        r#"{"request":{"cached_content":9},"reque\u0073t":{}}"#,
        r#"{"cachedContent":null,"cachedContent":"projects/alpha/cachedContents/x"}"#,
        r#"{"cached_content":"cachedContents/x","cached_content":"projects/alpha/cachedContents/x"}"#,
        r#"{"request":{"cachedContent":"projects/beta/cachedContents/x","cached\u0043ontent":"projects/alpha/cachedContents/x"}}"#,
        r#"{"request":{"cached_content":"projects/alpha/cachedContents/x","cached_content":"projects/alpha/cachedContents/x"}}"#,
    ] {
        let response = http
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{body}");
        assert_eq!(response.json::<Value>().await.unwrap()["error"]["status"], "INVALID_ARGUMENT");
    }
    assert!(proxy.state.read().await.activity.is_empty());
}

#[tokio::test]
async fn google_rejects_non_string_project_aliases_before_dispatch() {
    let mock = serve(Router::new().route("/v1internal:generateContent", post(|| async { "dispatched" }))).await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    for (alias, other) in [("project", "cloudaicompanionProject"), ("cloudaicompanionProject", "project")] {
        for value in [Value::Null, json!(123), json!({}), json!([]), json!(false)] {
            let body = json!({(alias): value, (other):"project-ok"});
            let res =
                http.request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").json(&body).send().await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(res.json::<Value>().await.unwrap()["error"]["status"], "INVALID_ARGUMENT");
        }
    }
    assert!(proxy.state.read().await.activity.is_empty());
}

#[tokio::test]
async fn google_preoutput_errors_obey_http_retry_policy_and_return_json() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
            if router::bearer(&headers) == Some("google-b.json") {
                return "alternate".into_response();
            }
            let error = &body["error"];
            let early = json!({"usageMetadata":{"cachedContentTokenCount":20,"totalTokenCount":110}});
            let late = json!({"usageMetadata":{"promptTokenCount":100}});
            if body["sse"] == true {
                ([("content-type", "text/event-stream")], format!("data: {early}\n\ndata: {late}\n\ndata: {error}\n\n")).into_response()
            } else {
                axum::Json(json!([early, late, error])).into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    for (code, http_code, rpc, sse, switch, retry) in [
        (3, 400, "INVALID_ARGUMENT", true, true, false),
        (403, 403, "PERMISSION_DENIED", false, true, false),
        (401, 401, "UNAUTHENTICATED", true, true, true),
        (503, 503, "UNAVAILABLE", false, false, false),
        (503, 503, "UNAVAILABLE", true, true, true),
    ] {
        {
            let mut state = proxy.state.write().await;
            state.pool.policy.switch_on_server_error = switch;
            state.pool.overrides.insert(Lane::Google, "google-a.json".into());
            state.pool.accounts.get_mut("google-a.json").unwrap().auth_cooldown = None;
        }
        let error = json!({"error":{"code":code,"status":rpc,"message":"fixture"}});
        let res = http
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"error":error,"sse":sse}))
            .send()
            .await
            .unwrap();
        if retry {
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(res.text().await.unwrap(), "alternate");
        } else {
            assert_eq!(res.status(), http_code);
            assert_eq!(res.headers()["content-type"], "application/json");
            assert_eq!(res.json::<Value>().await.unwrap(), error);
        }
        let state = proxy.state.read().await;
        assert_eq!(state.pool.accounts["google-a.json"].auth_cooldown.is_some(), http_code == 401);
        let row = state.activity.back().unwrap();
        assert_eq!(row.attempts.len(), if retry { 2 } else { 1 });
        let attempt = &row.attempts[0];
        assert_eq!((attempt.input_tokens, attempt.output_tokens, attempt.cache_read_tokens), (100, 10, 20));
    }
}

#[tokio::test]
async fn google_first_byte_ends_timeout_even_when_candidates_arrive_after_inspection_bound() {
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let release = release.clone();
            move |headers: HeaderMap| {
                let release = release.clone();
                async move {
                    if router::bearer(&headers) != Some("google-a.json") {
                        return "unexpected replay".into_response();
                    }
                    let meta = futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(700)).await;
                        Ok::<_, std::io::Error>(": heartbeat\n\ndata: {\"usageMetadata\":{\"promptTokenCount\":8}}\n\ndata: {")
                    });
                    let output = futures_util::stream::once(async move {
                        release.notified().await;
                        Ok("\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"same attempt\"}]}}]}\n\n")
                    });
                    ([("content-type", "text/event-stream")], Body::from_stream(futures_util::StreamExt::chain(meta, output)))
                        .into_response()
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 1;
    let server = serve(proxy.clone().app()).await;
    let mut res = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let mut prefix = Vec::new();
    let completed = b": heartbeat\n\ndata: {\"usageMetadata\":{\"promptTokenCount\":8}}\n\n";
    while prefix.len() < completed.len() {
        prefix.extend_from_slice(&res.chunk().await.unwrap().expect("same stream must forward its completed frames"));
    }
    assert_eq!(prefix, completed);
    assert!(prefix.starts_with(b": heartbeat"));
    release.notify_one();
    assert_eq!(res.text().await.unwrap(), "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"same attempt\"}]}}]}\n\n");
    let state = proxy.state.read().await;
    assert_eq!(state.activity.back().unwrap().attempts.len(), 1);
    assert_eq!(state.activity.back().unwrap().input_tokens, 8);
}

#[tokio::test]
async fn google_flushes_cr_only_sse_heartbeat_before_a_stall() {
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let release = release.clone();
            move || async move {
                let heartbeat = futures_util::stream::iter([Ok::<_, std::io::Error>(": ping\r\r")]);
                let tail = futures_util::stream::once(async move {
                    release.notified().await;
                    Ok("\ndata: {\"usageMetadata\":{\"promptTokenCount\":8}}\r\ndata: {}\n\n")
                });
                ([("content-type", "text/event-stream")], Body::from_stream(futures_util::StreamExt::chain(heartbeat, tail)))
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let mut response = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut prefix = Vec::new();
    while prefix.len() < b": ping\r\r".len() {
        prefix.extend_from_slice(&response.chunk().await.unwrap().unwrap());
    }
    assert_eq!(prefix, b": ping\r\r");
    release.notify_one();
    assert_eq!(response.text().await.unwrap(), "\ndata: {\"usageMetadata\":{\"promptTokenCount\":8}}\r\ndata: {}\n\n");
    assert_eq!(proxy.state.read().await.activity.back().unwrap().input_tokens, 8);
}

#[tokio::test]
async fn google_inspection_timeout_keeps_partial_array_error_rewritable() {
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let release = release.clone();
            move || async move {
                let prefix = futures_util::stream::iter([Ok::<_, std::io::Error>("[{}, \n {\"error\":")]);
                let tail = futures_util::stream::once(async move {
                    release.notified().await;
                    Ok("{\"code\":8}}]")
                });
                ([("content-type", "application/json")], Body::from_stream(futures_util::StreamExt::chain(prefix, tail)))
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let mut res = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let mut prefix = Vec::new();
    while prefix.len() < b"[{}, \n ".len() {
        prefix.extend_from_slice(&res.chunk().await.unwrap().unwrap());
    }
    assert_eq!(prefix, b"[{}, \n ");
    release.notify_one();
    prefix.extend(res.bytes().await.unwrap());
    let body: Value = serde_json::from_slice(&prefix).unwrap();
    assert_eq!(body[1]["error"]["code"], 429);
    let details = body[1]["error"]["details"].as_array().unwrap();
    assert_eq!(details.len(), 1);
    assert_eq!(details[0]["reason"], "QUOTA_EXHAUSTED");
    assert_eq!(proxy.state.read().await.activity.back().unwrap().attempts.len(), 1);
}

#[tokio::test]
async fn google_midstream_quota_normalizes_stale_resets_and_inspects_after_oversized_items() {
    let small = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"test","args":{}}}]}}]});
    let huge = json!({"candidates":[{"content":{"parts":[{"text":format!("{}\\\" ] }}", "x".repeat(1024 * 1024))}]}}]});
    let stale = json!({"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[
        {"@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetTimeStamp":"2000-01-01T00:00:00Z","model":"gemini"}},
        {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"0s"}
    ]}});
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let (small, huge, stale) = (small.clone(), huge.clone(), stale.clone());
            move |axum::Json(body): axum::Json<Value>| {
                let (small, huge, stale) = (small.clone(), huge.clone(), stale.clone());
                async move {
                    let (kind, first, second, third) = if body["large"] == true {
                        ("application/json", format!("[{small},\n"), format!("{huge},\n"), format!("{stale}]"))
                    } else {
                        ("text/event-stream", format!("data: {small}\r\n\r\n"), String::new(), format!("data: {stale}\r\n\r\n"))
                    };
                    let chunks = [first, second, third].map(Ok::<_, std::io::Error>);
                    ([("content-type", kind)], Body::from_stream(futures_util::stream::iter(chunks)))
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.overrides.insert(Lane::Google, "google-a.json".into());
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    for large in [false, true] {
        proxy.state.write().await.pool.accounts.get_mut("google-a.json").unwrap().scoped_cooldowns.clear();
        let res = http
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"model":"gemini-4-neon-high","large":large}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body = res.text().await.unwrap();
        let error = if large {
            assert!(body.starts_with(&format!("[{small},\n{huge},\n")));
            let array: Value = serde_json::from_str(&body).unwrap();
            array[2].clone()
        } else {
            assert!(body.starts_with(&format!("data: {small}\r\n\r\n")));
            serde_json::from_str(body.lines().nth(2).unwrap().strip_prefix("data: ").unwrap()).unwrap()
        };
        let details = error["error"]["details"].as_array().unwrap();
        assert_eq!(details.len(), 2);
        assert!(details[0]["metadata"].get("quotaResetTimeStamp").is_none(), "{details:?}");
        assert_eq!(details[0]["metadata"]["model"], "gemini");
        assert!(details[0]["metadata"].get("quotaResetDelay").is_none());
        assert!(details[1].get("retryDelay").is_none());
        let state = proxy.state.read().await;
        assert!(!state.pool.accounts["google-a.json"].scoped_cooldowns["gemini"].verified_reset);
        let row = state.activity.back().unwrap();
        assert_eq!(row.status_code, Some(429));
        assert_eq!(row.attempts.len(), 1);
    }
}

#[tokio::test]
async fn google_midstream_quota_keeps_native_delay_without_exporting_the_longer_account_cooldown() {
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let release = release.clone();
            move || {
                let release = release.clone();
                async move {
                    let output = futures_util::stream::once(async {
                        Ok::<_, std::io::Error>(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"started\"}]}}]}\n\n".as_slice())
                    });
                    let error = futures_util::stream::once(async move {
                        release.notified().await;
                        Ok(br#"data: {"error":{"code":8,"details":[{"@type":"google.rpc.ErrorInfo","metadata":{"quotaResetTimeStamp":"2000-01-01T00:00:00Z","quotaResetDelay":"30s","model":"gemini"}},{"@type":"google.rpc.RetryInfo","retryDelay":"30s"},{"@type":"other","keep":true}]}}

"#.as_slice())
                    });
                    ([("content-type", "text/event-stream")], Body::from_stream(futures_util::StreamExt::chain(output, error)))
                }
            }
        }),
    )).await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let mut res = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(4))
        .json(&json!({"model":"gemini-4-neon-high"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let first = res.chunk().await.unwrap().unwrap();
    assert!(first.starts_with(b"data: {\"candidates\""));
    let reset = DateTime::from_timestamp(Utc::now().timestamp() + 120, 500_000_000).unwrap();
    proxy
        .state
        .write()
        .await
        .pool
        .accounts
        .get_mut("google-a.json")
        .unwrap()
        .scoped_cooldowns
        .insert("gemini".into(), pool::Cooldown { until: reset, verified_reset: true });
    release.notify_one();
    let body = res.text().await.unwrap();
    let raw: Value = serde_json::from_str(body.lines().find_map(|line| line.strip_prefix("data: ")).unwrap()).unwrap();
    let details = &raw["error"]["details"];
    let metadata = &details[0]["metadata"];
    assert!(metadata.get("quotaResetTimeStamp").is_none());
    assert_eq!(metadata["quotaResetDelay"], "30s");
    assert_eq!(details[1]["retryDelay"], "30s");
    assert_eq!(details[2]["keep"], true);
    let state = proxy.state.read().await;
    assert_eq!(state.pool.accounts["google-a.json"].scoped_cooldowns["gemini"].until, reset);
    assert_eq!(state.activity.back().unwrap().attempts.len(), 1);
}

#[tokio::test]
async fn google_usage_infers_output_from_final_inclusive_input() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|| async {
            (
                [("content-type", "text/event-stream")],
                concat!(
                    "data: {\"usageMetadata\":{\"cachedContentTokenCount\":20,\"totalTokenCount\":110}}\n\n",
                    "data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10,\"totalTokenCount\":110}}\n\n",
                ),
            )
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let state = proxy.state.read().await;
    let row = state.activity.back().unwrap();
    assert_eq!((row.input_tokens, row.output_tokens, row.cache_read_tokens), (100, 10, 20));
    assert_eq!(row.attempts[0].output_tokens, 10);
}

#[tokio::test]
async fn google_terminal_429_waits_past_short_header_budget_until_error_object_completes() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|| async {
            let body = futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_millis(700)).await;
                Ok::<_, std::io::Error>(r#"{"error":{"details":[{"@type":"google.rpc.RetryInfo","retryDelay":"120s"}]}}"#)
            });
            let body = Body::from_stream(futures_util::StreamExt::chain(body, futures_util::stream::pending()));
            (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], body)
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    {
        let mut state = proxy.state.write().await;
        state.pool.accounts.remove("google-b.json");
        state.pool.policy.first_byte_timeout_secs = 4;
    }
    let server = serve(proxy.clone().app()).await;
    let res = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(3))
        .json(&json!({"model":"gemini-4-neon-high"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers()["retry-after"].to_str().unwrap().parse::<u64>().unwrap() >= 115);
    assert!(
        proxy.state.read().await.pool.accounts["google-a.json"].scoped_cooldowns["gemini"].until > Utc::now() + TimeDelta::seconds(115)
    );
}

#[tokio::test]
async fn google_terminal_429_keeps_an_expired_header_reset_when_the_body_stalls() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|| async {
            let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
            (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], body)
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    {
        let mut state = proxy.state.write().await;
        state.pool.accounts.remove("google-b.json");
        state.pool.policy.first_byte_timeout_secs = 2;
    }
    let server = serve(proxy.app()).await;
    let res = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert_eq!(res.headers()["x-should-retry"], "false");
    assert_eq!(res.headers()["retry-after"], "5");
    assert!(!res.headers().contains_key(router::POOL_QUOTA_HEADER));
    assert_google_pool_body(&res.json().await.unwrap(), "");
}

#[tokio::test]
async fn google_terminal_429_prefers_expired_header_over_later_peer_reset() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|headers: HeaderMap| async move {
            if router::bearer(&headers) == Some("google-a.json") {
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "3600")], "{}").into_response()
            } else {
                let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], body).into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 2;
    let server = serve(proxy.clone().app()).await;
    let response = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "5");
    assert!(!response.headers().contains_key(router::POOL_QUOTA_HEADER));
    assert_google_pool_body(&response.json().await.unwrap(), "");
    assert_eq!(proxy.state.read().await.activity.back().unwrap().attempts.len(), 2);
}

#[tokio::test]
async fn google_terminal_429_retains_an_expired_earlier_attempt_reset() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|headers: HeaderMap| async move {
            if router::bearer(&headers) == Some("google-a.json") {
                let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], body).into_response()
            } else {
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "3600")], "{}").into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 2;
    let server = serve(proxy.clone().app()).await;
    let response = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "5");
    assert!(!response.headers().contains_key(router::POOL_QUOTA_HEADER));
    assert_google_pool_body(&response.json().await.unwrap(), "");
    let state = proxy.state.read().await;
    assert_eq!(state.activity.back().unwrap().attempts.len(), 2);
    assert!(state.pool.accounts["google-a.json"].quota_limited_until(Utc::now(), "").is_none());
}

#[tokio::test]
async fn google_terminal_429_prefers_live_cooldown_over_retained_reset_for_the_same_account() {
    let dispatched = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let (dispatched, release) = (dispatched.clone(), release.clone());
            move |headers: HeaderMap| async move {
                if router::bearer(&headers) == Some("google-a.json") {
                    let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
                    return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], body).into_response();
                }
                dispatched.notify_one();
                release.notified().await;
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")], "{}").into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 5;
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let send = http.request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").timeout(Duration::from_secs(3)).send();
    tokio::pin!(send);
    tokio::select! {
        seen = tokio::time::timeout(Duration::from_secs(2), dispatched.notified()) => seen.expect("alternate must dispatch"),
        reply = &mut send => panic!("reply before alternate dispatch: {reply:?}"),
    }
    proxy.state.write().await.pool.accounts.get_mut("google-a.json").unwrap().cooldown =
        Some(pool::Cooldown { until: Utc::now() + TimeDelta::seconds(120), verified_reset: false });
    release.notify_one();
    let response = send.await.unwrap();
    assert_eq!(response.status(), 429);
    let delay: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((58..=60).contains(&delay), "{delay}");
}

#[tokio::test]
async fn google_in_band_quota_normalizes_when_accounts_are_disabled_during_join() {
    assert_google_in_band_quota_after_accounts_disabled(None, 5).await;
}

#[tokio::test]
async fn google_in_band_quota_keeps_retry_after_when_accounts_are_disabled_during_join() {
    assert_google_in_band_quota_after_accounts_disabled(Some("120"), 120).await;
}

fn assert_google_pool_body(body: &Value, model: &str) {
    let mut detail =
        json!({"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED","domain":"cloudcode-pa.googleapis.com"});
    if !model.is_empty() {
        detail["metadata"] = json!({"model":model});
    }
    assert_eq!(
        *body,
        json!({"error":{"code":429,"message":"all eligible accounts are rate limited","status":"RESOURCE_EXHAUSTED",
            "details":[detail]}})
    );
}

fn google_witness(headers: &HeaderMap, model: &str) -> Option<DateTime<Utc>> {
    let header = headers.get(router::POOL_QUOTA_HEADER)?.to_str().unwrap();
    let (retry_at, wire_model) = header.strip_prefix("v1;retry_at=").unwrap().split_once(";wire_model=").unwrap();
    assert_eq!(wire_model, model);
    assert!(retry_at.ends_with('Z') && !retry_at.contains('.'));
    Some(retry_at.parse().unwrap())
}

async fn assert_google_in_band_quota_after_accounts_disabled(retry_after: Option<&'static str>, expected_delay: u64) {
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let release = release.clone();
            move |headers: HeaderMap| {
                let release = release.clone();
                async move {
                    if router::bearer(&headers) == Some("google-b.json") {
                        let mut response = axum::Json(json!({"error":{"code":8}})).into_response();
                        if let Some(value) = retry_after {
                            response.headers_mut().insert("retry-after", axum::http::HeaderValue::from_static(value));
                        }
                        return response;
                    }
                    let body = Body::from_stream(futures_util::stream::once(async move {
                        release.notified().await;
                        Ok::<_, std::io::Error>("{}")
                    }));
                    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], body).into_response()
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    {
        let mut state = proxy.state.write().await;
        state.pool.policy.first_byte_timeout_secs = 5;
        state.pool.policy.failover_cooldown_secs = 180;
    }
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let send = http
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .json(&json!({"model":"gemini-4-neon-high"}))
        .timeout(Duration::from_secs(6))
        .send();
    tokio::pin!(send);
    let decoded = async {
        while proxy.state.read().await.pool.accounts["google-b.json"].quota_limited_until(Utc::now(), "gemini-4-neon-high").is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        seen = tokio::time::timeout(Duration::from_secs(2), decoded) => seen.expect("alternate must record its decoded quota error"),
        reply = &mut send => panic!("reply before releasing the earlier body: {reply:?}"),
    }
    {
        let mut state = proxy.state.write().await;
        for account in state.pool.accounts.values_mut().filter(|a| a.lane == Lane::Google) {
            account.disabled = true;
        }
    }
    release.notify_one();
    let response = send.await.unwrap();
    assert_eq!(response.status(), 429);
    let delay: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((expected_delay.saturating_sub(1)..=expected_delay).contains(&delay), "{delay}");
    assert!(!response.headers().contains_key(router::POOL_QUOTA_HEADER));
    assert_google_pool_body(&response.json().await.unwrap(), "gemini-4-neon-high");
}

#[tokio::test]
async fn google_in_band_quota_survives_accounts_disabled_after_failover_preview() {
    for retry_after in [Some("120"), None] {
        let mock = serve(Router::new().route(
            "/v1internal:generateContent",
            post(move |headers: HeaderMap| async move {
                assert_eq!(router::bearer(&headers), Some("google-a.json"));
                let mut response = axum::Json(json!({"error":{"code":8}})).into_response();
                if let Some(value) = retry_after {
                    response.headers_mut().insert("retry-after", axum::http::HeaderValue::from_static(value));
                }
                response
            }),
        ))
        .await;
        let dir = Scratch::new();
        let proxy = google_routing_fixture(&dir, &mock).await;
        let app = proxy.app().layer(axum::middleware::from_fn(|request, next: axum::middleware::Next| {
            router::AFTER_FAILOVER_PREVIEW.scope(
                |pool| {
                    for account in pool.accounts.values_mut() {
                        account.disabled = true;
                    }
                },
                next.run(request),
            )
        }));
        let server = serve(app).await;
        let response = Http::new(&server)
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"model":"gemini-4-neon-high"}))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429, "upstream Retry-After: {retry_after:?}");
        let delay: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
        assert!(!response.headers().contains_key(router::POOL_QUOTA_HEADER));
        assert_google_pool_body(&response.json().await.unwrap(), "gemini-4-neon-high");
        if retry_after.is_some() {
            assert!((119..=120).contains(&delay), "{delay}");
        } else {
            assert_eq!(delay, 5);
        }
    }
}

#[tokio::test]
async fn google_in_band_quota_normalizes_after_pending_reader_outlasts_its_deadline() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|headers: HeaderMap| async move {
            if router::bearer(&headers) == Some("google-a.json") {
                let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], body).into_response()
            } else {
                axum::Json(json!({"error":{"code":8}})).into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    {
        let mut state = proxy.state.write().await;
        state.pool.policy.first_byte_timeout_secs = 3;
        state.pool.policy.failover_cooldown_secs = 1;
    }
    let server = serve(proxy.clone().app()).await;
    let response = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(5))
        .json(&json!({"model":"gemini-4-neon-high"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "5");
    assert!(!response.headers().contains_key(router::POOL_QUOTA_HEADER));
    assert_google_pool_body(&response.json().await.unwrap(), "gemini-4-neon-high");
    let state = proxy.state.read().await;
    assert_eq!(state.activity.back().unwrap().attempts.len(), 2);
    assert!(state.pool.next_reset(Lane::Google, "gemini-4-neon-high", None, Utc::now()).is_none());
}

#[tokio::test]
async fn google_terminal_429_waits_for_earlier_body_without_delaying_alternate_dispatch() {
    let release = Arc::new(Notify::new());
    let dispatched = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let (release, dispatched) = (release.clone(), dispatched.clone());
            move |headers: HeaderMap| async move {
                if router::bearer(&headers) == Some("google-b.json") {
                    dispatched.notify_one();
                    return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "40")], "{}").into_response();
                }
                let body = futures_util::stream::once(async move {
                    release.notified().await;
                    Ok::<_, std::io::Error>(r#"{"error":{"details":[{"@type":"google.rpc.RetryInfo","retryDelay":"120s"}]}}"#)
                });
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], Body::from_stream(body)).into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 4;
    let server = serve(proxy.app()).await;
    let http = Http::new(&server);
    let send = http.request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").timeout(Duration::from_secs(3)).send();
    tokio::pin!(send);
    tokio::select! {
        seen = tokio::time::timeout(Duration::from_secs(2), dispatched.notified()) => seen.expect("alternate must dispatch while the earlier body is stalled"),
        reply = &mut send => panic!("reply before alternate dispatch: {reply:?}"),
    }
    assert!(tokio::time::timeout(Duration::from_millis(150), &mut send).await.is_err());
    release.notify_one();
    let res = send.await.unwrap();
    assert_eq!(res.status(), 429);
    let delay: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((35..=40).contains(&delay), "{delay}");
}

#[tokio::test]
async fn google_terminal_429_does_not_wait_for_a_stalled_reader_with_a_later_reset() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|headers: HeaderMap| async move {
            if router::bearer(&headers) == Some("google-a.json") {
                let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "120")], body).into_response()
            } else {
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "40")], "{}").into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 6;
    let server = serve(proxy.app()).await;
    let res = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    let delay: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((35..=40).contains(&delay), "{delay}");
}

#[tokio::test]
async fn google_failover_reads_delayed_429_body_and_extends_cooldown_once() {
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let release = release.clone();
            move |headers: HeaderMap| {
                let release = release.clone();
                async move {
                    if router::bearer(&headers) == Some("google-b.json") {
                        return "alternate".into_response();
                    }
                    let body = futures_util::stream::once(async move {
                        release.notified().await;
                        Ok::<_, std::io::Error>(r#"{"error":{"details":[{"@type":"google.rpc.RetryInfo","retryDelay":"120s"}]}}"#)
                    });
                    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], Body::from_stream(body)).into_response()
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 4;
    let server = serve(proxy.clone().app()).await;
    let res = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(3))
        .json(&json!({"model":"gemini-4-neon-high"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), "alternate");
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            {
                let state = proxy.state.read().await;
                let failed = &state.pool.accounts["google-a.json"];
                if failed.scoped_cooldowns["gemini"].until > Utc::now() + TimeDelta::seconds(115) {
                    assert_eq!(failed.failure, 1);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the delayed 429 body must extend the failed account's cooldown");
}

#[tokio::test]
async fn google_ignores_shared_numeric_sessions_and_rebinds_explicit_affinity_on_429() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let refreshes = Arc::new(AtomicUsize::new(0));
    let refreshed = Arc::new(Notify::new());
    let mock = serve(
        Router::new()
            .route(
                "/v1internal:streamGenerateContent",
                post(|headers: HeaderMap, axum::Json(raw): axum::Json<Value>| async move {
                    let token = router::bearer(&headers).unwrap();
                    if token == "google-a.json" && raw["limit"] == true {
                        let raw = json!({"error":{"details":[{
                            "@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetDelay":"120.5s"}
                        }] }});
                        (StatusCode::TOO_MANY_REQUESTS, axum::Json(raw)).into_response()
                    } else {
                        token.to_owned().into_response()
                    }
                }),
            )
            .route("/v1beta/models/{action}", post(|headers: HeaderMap| async move { router::bearer(&headers).unwrap().to_owned() }))
            .route(
                "/v1internal:retrieveUserQuotaSummary",
                post({
                    let refreshes = refreshes.clone();
                    let refreshed = refreshed.clone();
                    move || {
                        let refreshes = refreshes.clone();
                        let refreshed = refreshed.clone();
                        async move {
                            refreshes.fetch_add(1, Relaxed);
                            refreshed.notify_one();
                            axum::Json(json!({"buckets":[{"bucketId":"gemini-weekly","window":"weekly","remainingFraction":0.8}]}))
                        }
                    }
                }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let send = |header: &str, trajectory: Option<&str>, session_id: Option<&str>, limit| {
        http.request("POST", "/v1internal:streamGenerateContent").bearer_auth("inference-fixture").header("x-session-id", header).json(
            &json!({
                "model":"gemini-4-neon-high", "request":{"session_id":"-3750763034362895579"},
                "trajectoryId":trajectory, "session_id":session_id, "limit":limit
            }),
        )
    };
    const HUGE_NUMERIC: &str = "-999999999999999999999999999";
    for (header, trajectory, session_id, limit, expected) in [
        ("", None, None, false, "google-a.json"),
        ("-3750763034362895579", None, None, false, "google-b.json"),
        (HUGE_NUMERIC, Some("trajectory-a"), None, false, "google-a.json"),
        ("", None, Some("explicit-b"), false, "google-b.json"),
        ("explicit-b", None, None, false, "google-b.json"),
        ("", Some("trajectory-a"), None, true, "google-b.json"),
    ] {
        let res = send(header, trajectory, session_id, limit).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.text().await.unwrap(), expected);
    }
    for (model, expected) in [("gemini-4-neon-high", "google-b.json"), ("gemini-4-argon-high", "google-a.json")] {
        let response = http
            .request("POST", &format!("/v1beta/models/{model}:generateContent"))
            .bearer_auth("inference-fixture")
            .json(&json!({"model":"gemini-4-neon-high"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), expected);
    }
    tokio::time::timeout(Duration::from_secs(2), refreshed.notified()).await.unwrap();
    for _ in 0..2 {
        proxy.schedule_usage_refresh("google-a.json".into()).await;
    }
    let state = proxy.state.read().await;
    let a = &state.pool.accounts["google-a.json"];
    assert!(a.cooldown.is_none());
    assert!(a.scoped_cooldowns["gemini"].until > Utc::now() + TimeDelta::seconds(118));
    assert_eq!(state.pool.bound(Lane::Google, "-3750763034362895579"), None);
    assert_eq!(state.pool.bound(Lane::Google, HUGE_NUMERIC), None);
    assert_eq!(state.pool.bound(Lane::Google, "explicit-b"), Some("google-b.json"));
    assert_eq!(state.pool.bound(Lane::Google, "trajectory-a"), Some("google-b.json"));
    assert_eq!(state.activity.back().unwrap().requested_model, "gemini-4-argon-high");
    assert_eq!(state.usage_refreshes.len(), 1);
    assert_eq!(refreshes.load(Relaxed), 1);
}

#[tokio::test]
async fn google_terminal_429_uses_the_earliest_pool_reset_from_chunked_upstream_errors() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|headers: HeaderMap| async move {
            let (kind, detail) = if router::bearer(&headers) == Some("google-a.json") {
                ("RetryInfo", json!({"retryDelay":"37s"}))
            } else {
                ("ErrorInfo", json!({"metadata":{"quotaResetDelay":"2h15m3s"}}))
            };
            let mut detail = detail;
            detail["@type"] = json!(format!("type.googleapis.com/google.rpc.{kind}"));
            let body = json!({"error":{"details":[detail]}}).to_string();
            let body = Bytes::from(body);
            let split = body.len() / 2;
            let chunks = [body.slice(..split), body.slice(split..)].map(Ok::<_, std::io::Error>);
            (StatusCode::TOO_MANY_REQUESTS, Body::from_stream(futures_util::stream::iter(chunks)))
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let model = "gemini-4-neon-high";
    let mut witness = None;
    // The first call exhausts the pool on failover; the second cannot pick an account.
    for _ in 0..2 {
        let response = http
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"model":model}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["x-should-retry"], "false");
        let retry_after: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((34..=37).contains(&retry_after), "{retry_after}");
        let retry_at = google_witness(response.headers(), model).unwrap();
        assert!((34..=37).contains(&(retry_at - Utc::now()).num_seconds()));
        assert_eq!(*witness.get_or_insert(retry_at), retry_at);
        assert_google_pool_body(&response.json::<Value>().await.unwrap(), model);
    }
    let state = proxy.state.read().await;
    let final_exchange = state.activity.back().unwrap();
    assert_eq!(final_exchange.outcome, "rate_limited");
    assert!(final_exchange.attempts.is_empty());
    assert_eq!(state.activity[0].attempts.iter().map(|a| a.status_code).collect::<Vec<_>>(), [Some(429), Some(429)]);
    assert!(witness.unwrap() >= state.pool.accounts["google-a.json"].scoped_cooldowns["gemini"].until);
    assert!(state.pool.accounts["google-b.json"].scoped_cooldowns["gemini"].until > Utc::now() + TimeDelta::hours(2));
}

#[tokio::test]
async fn google_terminal_429_omits_unverified_witness_but_keeps_retry_after() {
    for (header, values, body, delay) in [
        ("retry-after", &["9999999"][..], "{}", 604_800),
        ("retry-after", &["+60"][..], r#"{"error":{"details":[{"@type":"RetryInfo","retryDelay":"90s"}]}}"#, 90),
        ("retry-after", &["invalid"][..], r#"{"error":{"details":[{"@type":"RetryInfo","retryDelay":"90s"}]}}"#, 90),
        ("retry-after", &["30", "60"][..], r#"{"error":{"details":[{"@type":"RetryInfo","retryDelay":"90s"}]}}"#, 90),
        ("x-ratelimit-reset-unknown", &["90"][..], "{}", 90),
    ] {
        let mock = serve(Router::new().route(
            "/v1internal:generateContent",
            post(move || async move {
                let mut response = (StatusCode::TOO_MANY_REQUESTS, body).into_response();
                for value in values {
                    response.headers_mut().append(header, axum::http::HeaderValue::from_static(value));
                }
                response
            }),
        ))
        .await;
        let dir = Scratch::new();
        let proxy = google_routing_fixture(&dir, &mock).await;
        let http_server = serve(proxy.app()).await;
        let http = Http::new(&http_server);
        let response = http
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"model":"gemini-neon"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert!(!response.headers().contains_key(router::POOL_QUOTA_HEADER), "{header}: {values:?}");
        let retry_after: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((delay - 5..=delay).contains(&retry_after), "{retry_after}");
        let body = response.json::<Value>().await.unwrap();
        assert_eq!(body["error"]["status"], "RESOURCE_EXHAUSTED");
        assert_eq!(body["error"]["details"][0]["reason"], "QUOTA_EXHAUSTED");
    }
}

#[tokio::test]
async fn google_routing_witness_and_usage_windows_share_model_validation() {
    let mock = serve(Router::new()).await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let until = Utc::now() + TimeDelta::minutes(2);
    for account in proxy.state.write().await.pool.accounts.values_mut().filter(|a| a.lane == Lane::Google) {
        account.cooldown = Some(pool::Cooldown { until, verified_reset: true });
    }
    let server = serve(proxy.app()).await;
    let http = Http::new(&server);
    for (model, query, valid) in [
        ("x?y", "x%3Fy", false),
        ("x;y", "x%3By", false),
        ("x y", "x%20y", false),
        ("x\ny", "x%0Ay", false),
        (&"x".repeat(193), &"x".repeat(193), false),
        (&"x".repeat(192), &"x".repeat(192), true),
    ] {
        let response = http
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"model":model}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers().contains_key("x-fleetix-pool-quota"), valid, "{model:?}");
        let diagnostic = format!("/v8/management/usage/windows?lane=google&model={query}");
        assert_eq!(http.management("GET", &diagnostic, None).await.status(), if valid { 200 } else { 400 }, "{model:?}");
    }
}

#[test]
fn cached_quota_reset_cannot_be_verified_before_it_was_observed() {
    use integrations::providers::antigravity::parse_antigravity_summary;
    let mut pool = Pool::default();
    let mut accounts = pool::parse_credentials("a.json", &json!({"type":"google","access_token":"fixture"}), false).unwrap();
    let now = Utc::now();
    let observed = now + TimeDelta::seconds(60);
    let reset = now + TimeDelta::minutes(10);
    accounts[0].windows =
        parse_antigravity_summary(&json!({"buckets":[{"bucketId":"gemini-5h","remainingFraction":0.0,"resetTime":reset}]}), observed);
    pool.insert(accounts);
    assert_eq!(pool.next_reset(Lane::Google, "gemini-neon", None, now), Some(reset));
    assert_eq!(pool.verified_google_reset_at("gemini-neon", None, now), None);
    assert_eq!(pool.verified_google_reset_at("gemini-neon", None, observed), Some(reset));
}

#[test]
fn duplicate_quota_reset_headers_only_set_legacy_backoff() {
    let now = Utc::now();
    let epoch = |seconds: i64| (now.timestamp() + seconds).to_string();
    for (lane, name, first, second, until) in [
        (Lane::Google, "retry-after", "60".into(), "600".into(), now + TimeDelta::seconds(60)),
        (Lane::Google, "x-ratelimit-reset", "60".into(), "600".into(), now + TimeDelta::seconds(600)),
        (Lane::Codex, "x-codex-primary-reset-at", epoch(60), epoch(600), DateTime::from_timestamp(now.timestamp() + 60, 0).unwrap()),
        (
            Lane::Claude,
            "anthropic-ratelimit-unified-reset",
            epoch(60),
            epoch(600),
            DateTime::from_timestamp(now.timestamp() + 600, 0).unwrap(),
        ),
    ] {
        let mut headers = HeaderMap::new();
        headers.append(name, first.parse().unwrap());
        assert!(router::header_cooldown(lane, &headers, now).unwrap().verified_reset);
        headers.append(name, second.parse().unwrap());
        let cooldown = router::header_cooldown(lane, &headers, now).unwrap();
        assert_eq!(cooldown.until, until);
        assert!(!cooldown.verified_reset, "{name}");
    }
}

#[test]
fn cached_quota_reset_must_be_in_range_when_observed() {
    use integrations::providers::antigravity::parse_antigravity_summary;
    let mut pool = Pool::default();
    pool.insert(pool::parse_credentials("a.json", &json!({"type":"google","access_token":"fixture"}), false).unwrap());
    let observed_at = Utc::now();
    let resets_at = observed_at + TimeDelta::days(8);
    let now = observed_at + TimeDelta::days(1);
    let model = "gemini-neon";
    let a = pool.accounts.get_mut("a.json").unwrap();
    a.windows = parse_antigravity_summary(
        &json!({"buckets":[{"bucketId":"gemini-5h","remainingFraction":0.0,"resetTime":resets_at}]}),
        observed_at,
    );
    assert_eq!(a.quota_limited_until(now, model), Some(resets_at));
    assert_eq!(pool.verified_google_reset_at(model, None, now), None);

    pool.accounts.get_mut("a.json").unwrap().windows[0].observed_at = Some(now);
    assert_eq!(pool.verified_google_reset_at(model, None, now), Some(resets_at));
}

#[test]
fn quota_decisions_agree_at_the_expiration_boundary() {
    let mut pool = Pool::default();
    pool.insert(pool::parse_credentials("a.json", &json!({"type":"google","access_token":"fixture"}), false).unwrap());
    let until = "2026-11-23T12:00:01Z".parse::<DateTime<Utc>>().unwrap();
    pool.accounts.get_mut("a.json").unwrap().cooldown = Some(pool::Cooldown { until, verified_reset: true });
    let model = "gemini-neon";
    for (now, reset) in [(until - TimeDelta::nanoseconds(1), Some(until)), (until, None)] {
        assert_eq!(pool.pick(Lane::Google, (None, None), model, None, &[], now).is_none(), reset.is_some());
        assert_eq!(pool.next_reset(Lane::Google, model, None, now), reset);
        assert_eq!(pool.all_quota_limited(Lane::Google, model, None, now), reset.is_some());
        assert_eq!(pool.verified_google_reset_at(model, None, now), reset);
    }
}

#[test]
fn verified_google_resets_require_complete_model_scoped_timing() {
    use contracts::providers::{ApiKeyOrder, QuotaUnit, QuotaWindowKind};
    use integrations::providers::antigravity::parse_antigravity_summary;
    use pool::Cooldown;
    let mut pool = Pool::default();
    let now = Utc::now();
    let at = |mins| now + TimeDelta::minutes(mins);
    let verified = |mins| Cooldown { until: at(mins), verified_reset: true };
    let accounts = |name| pool::parse_credentials(name, &json!({"type":"google","access_token":name}), false).unwrap();
    for name in ["a.json", "b.json", "disabled.json", "zero.json", "unsupported.json"] {
        pool.insert(accounts(name));
    }
    pool.insert(pool::parse_credentials("key.json", &json!({"type":"google","key":"fixture-key"}), false).unwrap());
    pool.policy.api_key_order = ApiKeyOrder::SubscriptionsOnly;
    pool.accounts.get_mut("disabled.json").unwrap().disabled = true;
    pool.accounts.get_mut("zero.json").unwrap().weight = 0;
    pool.accounts.get_mut("unsupported.json").unwrap().models = vec!["other".into()];
    let a = pool.accounts.get_mut("a.json").unwrap();
    a.cooldown = Some(verified(10));
    a.scoped_cooldowns = [("gemini", 30), ("gemini-argon", 15), ("3p", 40)].map(|(scope, mins)| (scope.into(), verified(mins))).into();
    let b = pool.accounts.get_mut("b.json").unwrap();
    b.cooldown = Some(verified(20));
    b.auth_cooldown = Some(now); // Expired auth cooldown is not a blocker.

    let next = |pool: &Pool, model| pool.verified_google_reset_at(model, None, now);
    const MODEL: &str = "gemini-neon";
    assert_eq!(next(&pool, MODEL), Some(at(20)));
    assert_eq!(next(&pool, "gemini-argon"), Some(at(15)));
    assert_eq!(next(&pool, "gpt-fixture"), Some(at(20)));
    for invalid in ["", " ", " model", "x\tmodel", "x;model", "x\u{7f}", "modèle"] {
        assert_eq!(next(&pool, invalid), None, "{invalid:?}");
    }
    let b = pool.accounts.get_mut("b.json").unwrap();
    b.auth_cooldown = Some(at(1));
    assert_eq!(next(&pool, MODEL), Some(at(30)));
    assert_eq!(next(&pool, "claude-fixture"), Some(at(40)));

    let windows = |reset: Option<DateTime<Utc>>| {
        parse_antigravity_summary(&json!({"buckets":[{"bucketId":"gemini-5h", "remainingFraction":0.0, "resetTime":reset}]}), now)
    };
    for (reset, expected) in [(Some(at(35)), Some(at(35))), (None, None), (Some(now + TimeDelta::days(8)), None), (Some(now), Some(at(30)))]
    {
        let a = pool.accounts.get_mut("a.json").unwrap();
        a.windows = windows(reset);
        assert_eq!(a.windows.len(), 1);
        assert_eq!(next(&pool, MODEL), expected, "{reset:?}");
    }
    let a = pool.accounts.get_mut("a.json").unwrap();
    a.windows = windows(None);
    let mut spend = a.windows[0].clone();
    spend.kind = QuotaWindowKind::Spend;
    a.windows[0].unit = QuotaUnit::Credits;
    a.windows.push(spend);
    assert_eq!(next(&pool, MODEL), Some(at(30)));
    let a = pool.accounts.get_mut("a.json").unwrap();
    a.windows.clear();
    a.cooldown = Some(Cooldown { verified_reset: false, ..verified(10) });
    let b = pool.accounts.get_mut("b.json").unwrap();
    b.auth_cooldown = None;
    assert_eq!(next(&pool, MODEL), Some(at(20)));
    let b = pool.accounts.get_mut("b.json").unwrap();
    b.cooldown = None;
    assert_eq!(next(&pool, MODEL), None); // B already usable.
}

#[test]
fn invalid_duplicate_retry_after_preserves_fallback_delay_without_proof() {
    let now = Utc::now();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.append("retry-after", "invalid".parse().unwrap());
    headers.append("retry-after", "600".parse().unwrap());
    headers.insert("x-ratelimit-reset", "60".parse().unwrap());
    assert_eq!(
        router::header_cooldown(Lane::Google, &headers, now),
        Some(pool::Cooldown { until: now + TimeDelta::seconds(60), verified_reset: false }),
    );
}

#[test]
fn cooldown_provenance_rejects_fallback_clamped_and_invalid_hints_and_survives_only_same_credential() {
    use pool::Cooldown;
    let now = DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
    let hint = |name, value: &str| {
        let headers = HeaderMap::from_iter([(axum::http::HeaderName::from_static(name), value.parse().unwrap())]);
        router::header_cooldown(Lane::Google, &headers, now)
    };
    let second = TimeDelta::seconds(1);
    for (value, until, expected) in [
        ("1s", now + second, true),
        ("7d", now + TimeDelta::days(7), true),
        ("7d1ms", now + TimeDelta::days(7), false),
        ("999ms", now + second, false),
        ("0", now + second, false),
        (&(now - second).to_rfc3339(), now + second, false),
        (&(now + TimeDelta::days(8)).to_rfc3339(), now + TimeDelta::days(7), false),
    ] {
        assert_eq!(hint("x-ratelimit-reset", value), Some(Cooldown { until, verified_reset: expected }), "{value}");
    }
    assert_eq!(hint("x-ratelimit-reset", "invalid"), None);
    let until = now + TimeDelta::minutes(1);
    for (name, value, verified_reset) in [
        ("x-ratelimit-reset-requests", "60s", true),
        ("x-ratelimit-reset-requests", "60", false),
        ("x-ratelimit-reset", "60.s", false),
        ("retry-after", &until.format("%a, %d %b %Y %H:%M:%S GMT").to_string(), true),
        ("retry-after", &until.to_rfc2822(), false),
    ] {
        assert_eq!(hint(name, value), Some(Cooldown { until, verified_reset }), "{name}: {value}");
    }
    let body = |delay| json!({"error":{"details":[{"@type":"RetryInfo","retryDelay":delay}]}}).to_string();
    let parse = |body: &str| router::body_cooldown(Lane::Google, body.as_bytes(), 25, now);
    let valid = parse(&body("25s"));
    let fallback = parse(&body("invalid"));
    assert!(valid.verified_reset);
    assert_eq!(fallback, Cooldown { until: valid.until, verified_reset: false });
    assert_eq!(valid.merge(fallback), fallback);
    assert_eq!(fallback.merge(valid), fallback);
    assert_eq!(valid.merge(parse(&body("0s"))), valid);
    assert_eq!(valid.merge(parse(&body("8d"))), Cooldown { until: now + TimeDelta::days(7), verified_reset: false });
    assert!(!parse(&body("0.99s")).verified_reset);

    let mut pool = Pool::default();
    let accounts = |token| pool::parse_credentials("a.json", &json!({"type":"google","access_token":token}), false).unwrap();
    pool.insert(accounts("initial"));
    let a = pool.accounts.get_mut("a.json").unwrap();
    a.cooldown = Some(valid);
    a.scoped_cooldowns.insert("gemini".into(), fallback);
    pool.insert(accounts("initial"));
    let a = &pool.accounts["a.json"];
    assert_eq!(a.cooldown, Some(valid));
    assert_eq!(a.scoped_cooldowns["gemini"], fallback);
    assert_eq!(pool.verified_google_reset_at("gemini-neon", None, now), None);
    assert_eq!(pool.verified_google_reset_at("gemini-neon", None, valid.until), None);
    pool.insert(accounts("rotated"));
    assert!(pool.accounts["a.json"].cooldown.is_none());
    assert!(pool.accounts["a.json"].scoped_cooldowns.is_empty());
}

#[test]
fn google_route_replacement_drops_cached_quota_and_cooldowns() {
    use integrations::providers::antigravity::parse_antigravity_summary;
    let now = Utc::now();
    let until = now + TimeDelta::minutes(5);
    let cooldown = pool::Cooldown { until, verified_reset: true };
    for (credential, field, before, after) in
        [("access_token", "project_id", "project-a", "project-b"), ("key", "base_url", "https://a.example.test", "https://b.example.test")]
    {
        let mut pool = Pool::default();
        let accounts = |route| {
            let mut raw = json!({"type":"google"});
            raw[credential] = json!("same-token");
            raw[field] = json!(route);
            pool::parse_credentials("a.json", &raw, false).unwrap()
        };
        pool.insert(accounts(before));
        let a = pool.accounts.get_mut("a.json").unwrap();
        a.cooldown = Some(cooldown);
        a.auth_cooldown = Some(until);
        a.scoped_cooldowns.insert("gemini".into(), cooldown);
        a.windows =
            parse_antigravity_summary(&json!({"buckets":[{"bucketId":"gemini-5h","remainingFraction":0.0,"resetTime":until}]}), now);
        pool.insert(accounts(after));
        let a = &pool.accounts["a.json"];
        assert!(a.cooldown.is_none() && a.auth_cooldown.is_none(), "{field}");
        assert!(a.scoped_cooldowns.is_empty() && a.windows.is_empty(), "{field}");
    }
}

#[tokio::test]
async fn google_in_flight_429_does_not_write_to_a_replaced_route() {
    for replace_project in [true, false] {
        let source = Arc::new(std::sync::OnceLock::<Arc<Proxy>>::new());
        let mock = serve(Router::new().route(
            "/v1internal:generateContent",
            post({
                let source = source.clone();
                move || {
                    let source = source.clone();
                    async move {
                        let mut state = source.get().unwrap().state.write().await;
                        let mut replacement = state.pool.accounts["google-a.json"].clone();
                        if replace_project {
                            replacement.project = Some("new-project".into());
                        } else {
                            replacement.base = Some(replacement.base.unwrap().join("new").unwrap());
                        }
                        state.pool.insert(vec![replacement]);
                        (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "60")])
                    }
                }
            }),
        ))
        .await;
        let dir = Scratch::new();
        let proxy = google_routing_fixture(&dir, &mock).await;
        {
            let mut state = proxy.state.write().await;
            state.pool.accounts.remove("google-b.json");
            if !replace_project {
                let a = state.pool.accounts.get_mut("google-a.json").unwrap();
                a.api_key = true;
                a.base = Some(mock.0.clone());
            }
        }
        assert!(source.set(proxy.clone()).is_ok());
        let server = serve(proxy.clone().app()).await;
        let response = Http::new(&server)
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"model":"gemini-neon"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 429);
        assert!(!response.headers().contains_key("x-fleetix-pool-quota"), "replace_project={replace_project}");
        let state = proxy.state.read().await;
        let a = &state.pool.accounts["google-a.json"];
        assert!(a.scoped_cooldowns.is_empty() && a.cooldown.is_none());
        assert_eq!(a.failure, 0);
    }
}

#[tokio::test]
async fn google_window_starter_preserves_provenance_and_quota_clear_removes_it() {
    use pool::Cooldown;
    let mock = serve(
        Router::new().route("/v1internal:generateContent", post(|| async { (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "300")]) })),
    )
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let account = {
        let mut state = proxy.state.write().await;
        let a = state.pool.accounts.get_mut("google-a.json").unwrap();
        a.scoped_cooldowns.insert("3p".into(), Cooldown { until: Utc::now(), verified_reset: true });
        a.clone()
    };
    let model = "gemini-neon";
    let request = proxy.http.post(mock.0.join("/v1internal:generateContent").unwrap());
    assert!(router::run_window_start(proxy.clone(), &account, model, request).await.is_err());
    {
        let state = proxy.state.read().await;
        let a = &state.pool.accounts["google-a.json"];
        assert!(!a.scoped_cooldowns.contains_key("3p"));
        assert!(a.scoped_cooldowns["gemini"].verified_reset);
    }
    let server = serve(proxy.clone().app()).await;
    let response =
        Http::new(&server).management("POST", "/v0/management/reset-quota", Some(json!({"auth_index":account.auth_index}))).await;
    assert_eq!(response.status(), 200);
    assert!(proxy.state.read().await.pool.accounts["google-a.json"].scoped_cooldowns.is_empty());
}

#[tokio::test]
async fn google_terminal_429_does_not_attest_cached_window_ttl() {
    use integrations::providers::antigravity::parse_antigravity_summary;
    let mock = serve(Router::new()).await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let now = Utc::now();
    for a in proxy.state.write().await.pool.accounts.values_mut().filter(|a| a.lane == Lane::Google) {
        a.windows = parse_antigravity_summary(&json!({"buckets":[{"bucketId":"gemini-5h","remainingFraction":0.0}]}), now);
    }
    let server = serve(proxy.app()).await;
    let response = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .json(&json!({"model":"gemini-neon"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert!(!response.headers().contains_key("x-fleetix-pool-quota"));
    let retry_after: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((3595..=3600).contains(&retry_after), "{retry_after}");
    assert_google_pool_body(&response.json().await.unwrap(), "gemini-neon");
}

#[tokio::test]
async fn a_passthrough_429_preserves_bytes_beyond_the_parse_limit() {
    let source = Arc::new(std::sync::OnceLock::<Arc<Proxy>>::new());
    let body = Bytes::from(vec![b'x'; 70 * 1024]);
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let source = source.clone();
            let body = body.clone();
            move || {
                let source = source.clone();
                let body = body.clone();
                async move {
                    // The credential disappeared while the request was in flight; there is no pool reset.
                    let proxy = source.get().unwrap();
                    proxy.state.write().await.pool.accounts.retain(|_, account| account.lane != Lane::Google);
                    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "37"), ("x-fleetix-pool-quota", "spoofed")], body)
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    assert!(source.set(proxy.clone()).is_ok());
    let server = serve(proxy.app()).await;
    let response = Http::new(&server).request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").send().await.unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["retry-after"], "37");
    assert!(!response.headers().contains_key("x-fleetix-pool-quota"));
    assert_eq!(response.bytes().await.unwrap(), body);
}

#[test]
fn cooldown_parsers_prioritize_exhausted_provider_windows() {
    let base = Utc::now();
    let early = DateTime::from_timestamp((base + TimeDelta::minutes(2)).timestamp(), 0).unwrap();
    let late = DateTime::from_timestamp((base + TimeDelta::hours(40)).timestamp(), 0).unwrap();
    let fields = |headers: &[(&'static str, &str)]| -> HeaderMap {
        headers.iter().map(|(name, value)| (axum::http::HeaderName::from_static(name), value.parse().unwrap())).collect()
    };
    assert_eq!(
        router::cooldown(
            Lane::Google,
            &HeaderMap::new(),
            &serde_json::to_vec(&json!({"error":{"details":[{
                "@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetTimeStamp":early.to_rfc3339()}
            }]}}))
            .unwrap(),
            5,
        ),
        early
    );
    let early_epoch = early.timestamp().to_string();
    let late_epoch = late.timestamp().to_string();
    for (primary, secondary, expected) in [("10", "100", late), ("100", "10", early), ("10", "20", early)] {
        let headers = fields(&[
            ("x-codex-primary-used-percent", primary),
            ("x-codex-secondary-used-percent", secondary),
            ("x-codex-primary-reset-at", &early_epoch),
            ("x-codex-secondary-reset-at", &late_epoch),
        ]);
        assert_eq!(router::cooldown(Lane::Codex, &headers, b"", 5), expected);
    }
    let mut headers = fields(&[
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-reset", &early_epoch),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-reset", &late_epoch),
    ]);
    assert_eq!(router::cooldown(Lane::Claude, &headers, b"", 5), early);
    headers.insert("anthropic-ratelimit-unified-7d-remaining", "0".parse().unwrap());
    assert_eq!(router::cooldown(Lane::Claude, &headers, b"", 5), late);
}

#[test]
fn cooldowns_bound_absolute_and_relative_resets_and_unknown_google_scopes() {
    use std::collections::BTreeSet;
    let scopes = (0..300).map(|n| pool::model_cooldown_scope(Lane::Google, &format!("unknown-{n}")).unwrap()).collect::<BTreeSet<_>>();
    assert_eq!(scopes, BTreeSet::from(["other".to_owned()]));

    let now = Utc::now();
    let distant = now + TimeDelta::days(90);
    let past = now - TimeDelta::seconds(90);
    for (lane, name, value, bound) in [
        (Lane::Google, "retry-after", "9999999".into(), TimeDelta::days(7)),
        (Lane::Google, "retry-after", distant.to_rfc2822(), TimeDelta::days(7)),
        (Lane::Google, "x-ratelimit-reset", distant.to_rfc3339(), TimeDelta::days(7)),
        (Lane::Claude, "anthropic-ratelimit-unified-reset", distant.timestamp().to_string(), TimeDelta::days(7)),
        (Lane::Codex, "x-codex-primary-reset-at", distant.timestamp().to_string(), TimeDelta::days(7)),
        (Lane::Claude, "anthropic-ratelimit-unified-reset", past.timestamp().to_string(), TimeDelta::seconds(1)),
        (Lane::Google, "x-ratelimit-reset", "0s".into(), TimeDelta::seconds(1)),
    ] {
        let headers = HeaderMap::from_iter([(axum::http::HeaderName::from_static(name), value.parse().unwrap())]);
        let at = router::cooldown(lane, &headers, b"", 30);
        assert!(at >= now + bound && at <= Utc::now() + bound, "{name}: {at}");
    }
    let body = json!({"error":{"details":[{"@type":"ErrorInfo","metadata":{"quotaResetTimeStamp":distant}}]}});
    let at = router::cooldown(Lane::Google, &HeaderMap::new(), body.to_string().as_bytes(), 30);
    assert!(at >= now + TimeDelta::days(7) && at <= Utc::now() + TimeDelta::days(7));
}

#[tokio::test]
async fn oauth_file_reload_on_401_retries_once_and_fails_over_to_an_expired_token_reloaded_from_disk() {
    use std::sync::Mutex as SyncMutex;
    let hits = Arc::new(SyncMutex::new(Vec::new()));
    let mock = serve(Router::new().route(
        "/{*endpoint}",
        post({
            let hits = hits.clone();
            move |headers: HeaderMap| {
                let hits = hits.clone();
                async move {
                    let token = router::bearer(&headers).unwrap();
                    hits.lock().unwrap().push(token.to_owned());
                    if token == "b-fresh" { StatusCode::OK } else { StatusCode::UNAUTHORIZED }
                }
            }
        }),
    ))
    .await;
    for (lane, path) in [(Lane::Claude, "/v1/messages"), (Lane::Codex, "/backend-api/codex/responses")] {
        hits.lock().unwrap().clear();
        let dir = Scratch::new();
        let proxy = fixture(&dir, &mock);
        let credentials = |name: &str, token: &str, expires_at| {
            json!({
                "type":lane.name(), "access_token":token, "refresh_token":format!("refresh-{token}"), "expires_at":expires_at,
                "priority":if name == "a.json" { 10 } else { 0 },
            })
        };
        let at = Utc::now();
        {
            let mut state = proxy.state.write().await;
            state.pool.policy.strategy = SoonestReset;
            state.pool.policy.failover_cooldown_secs = 600;
            for (name, old, fresh, expires) in
                [("a.json", "a-old", "a-refreshed", at + TimeDelta::hours(1)), ("b.json", "b-expired", "b-fresh", at - TimeDelta::hours(1))]
            {
                state.pool.insert(pool::parse_credentials(name, &credentials(name, old, expires), false).unwrap());
                let file = proxy.config.auth_dir.join(name);
                fs::write(file, credentials(name, fresh, at + TimeDelta::hours(1)).to_string()).unwrap();
            }
        }
        let server = serve(proxy.clone().app()).await;
        let response = Http::new(&server)
            .request("POST", path)
            .bearer_auth("inference-fixture")
            .json(&json!({"model":"fixture"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let _ = response.bytes().await.unwrap();
        assert_eq!(&*hits.lock().unwrap(), &["a-old", "a-refreshed", "b-fresh"]);
        let state = proxy.state.read().await;
        let a = &state.pool.accounts["a.json"];
        let until = a.auth_cooldown.unwrap();
        assert!(until > at + TimeDelta::seconds(55) && until <= Utc::now() + TimeDelta::seconds(60));
        assert!(a.cooldown.is_none());
        assert!(state.pool.next_reset(lane, "fixture", None, Utc::now()).is_none());
        assert_eq!(a.priority, 10);
        assert_eq!(a.access, "a-refreshed");
        assert_eq!(state.pool.accounts["b.json"].access, "b-fresh");
    }
}

#[tokio::test]
async fn codex_reload_rejects_changed_or_missing_enrolled_principal() {
    let mock = serve(Router::new()).await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let credential = |version, account, user| {
        let claims = json!({"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_user_id":user},"version":version});
        let token = format!("fixture.{}.unsigned", URL_SAFE_NO_PAD.encode(claims.to_string()));
        json!({"type":"codex","access_token":token})
    };
    let name = "codex.json";
    let accounts = pool::parse_credentials(name, &credential(0, Some("workspace"), Some("user")), false).unwrap();
    let enrolled = accounts[0].clone();
    proxy.state.write().await.pool.insert(accounts);
    for (account, user) in
        [(Some("workspace"), Some("other")), (Some("other"), Some("user")), (None, Some("user")), (Some("workspace"), None)]
    {
        let raw = credential(1, account, user);
        fs::write(proxy.config.auth_dir.join(name), raw.to_string()).unwrap();
        assert!(proxy.token(&enrolled, true).await.is_err());
        assert_eq!(proxy.state.read().await.pool.accounts[name].access, enrolled.access);
    }
    let mut raw = credential(1, Some("other"), Some("user"));
    raw["account_id"] = json!("workspace");
    fs::write(proxy.config.auth_dir.join(name), raw.to_string()).unwrap();
    assert!(proxy.token(&enrolled, true).await.is_err());
    let raw = credential(2, Some("workspace"), Some("user"));
    fs::write(proxy.config.auth_dir.join(name), raw.to_string()).unwrap();
    assert_eq!(proxy.token(&enrolled, true).await.unwrap(), raw["access_token"].as_str().unwrap());
}

#[tokio::test]
async fn google_refresh_does_not_install_tokens_into_replaced_refresh_only_credentials() {
    let began = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/test/google-token",
        post({
            let (began, release) = (began.clone(), release.clone());
            move || {
                let (began, release) = (began.clone(), release.clone());
                async move {
                    began.notify_one();
                    release.notified().await;
                    axum::Json(json!({"access_token":"old-account-token"}))
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let credential = |refresh| {
        pool::parse_credentials(
            "google.json",
            &json!({"type":"google","refresh_token":refresh,"client_id":"client","client_secret":"secret"}),
            false,
        )
        .unwrap()
        .remove(0)
    };
    let original = credential("first");
    proxy.state.write().await.pool.insert(vec![original.clone()]);
    let replace = async {
        began.notified().await;
        proxy.state.write().await.pool.insert(vec![credential("second")]);
        release.notify_one();
    };
    let (refresh, ()) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(proxy.token(&original, false), replace) }).await.unwrap();
    assert!(refresh.is_err());
    let state = proxy.state.read().await;
    assert!(state.pool.accounts["google.json"].access.is_empty());
    assert_eq!(state.pool.accounts["google.json"].refresh.as_deref(), Some("second"));
}

#[tokio::test]
async fn google_exhausted_failover_uses_quota_but_nonretryable_errors_keep_upstream_status() {
    let mock = serve(
        Router::new()
            .route(
                "/v1internal:generateContent",
                post(|headers: HeaderMap| async move {
                    if router::bearer(&headers) == Some("google-a.json") {
                        (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "30")], "limited")
                    } else {
                        (StatusCode::BAD_GATEWAY, [("x-request-id", "fixture-gateway")], "upstream gateway failure")
                    }
                }),
            )
            .route("/v1/messages", post(|| async { (StatusCode::UNAUTHORIZED, "upstream login rejected") })),
    )
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    {
        let mut state = proxy.state.write().await;
        state.pool.accounts.remove("b.json");
        state.pool.policy.switch_on_server_error = false;
    }
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let google = http.request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").send().await.unwrap();
    assert_eq!(google.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(google.headers()["x-request-id"], "fixture-gateway");
    assert_eq!(google.text().await.unwrap(), "upstream gateway failure");
    proxy.state.write().await.pool.policy.switch_on_server_error = true;
    let google = http.request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").send().await.unwrap();
    assert_eq!(google.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(google.json::<Value>().await.unwrap()["error"]["status"], "RESOURCE_EXHAUSTED");
    let claude = || http.request("POST", "/v1/messages").bearer_auth("inference-fixture").json(&json!({"model":"claude-fixture"}));
    assert_eq!(claude().send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let response = claude().send().await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key("x-should-retry"));
    let state = proxy.state.read().await;
    assert!(state.pool.accounts["a.json"].auth_cooldown.is_some());
    let now = Utc::now();
    assert!(state.pool.next_reset(Lane::Claude, "claude-fixture", None, now).is_none());
    assert!(!state.pool.all_quota_limited(Lane::Google, "", None, now));
}

#[tokio::test]
async fn google_forbidden_headers_survive_a_stall_and_broken_first_body_chunk() {
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let release = release.clone();
            move |headers: HeaderMap| async move {
                if router::bearer(&headers) != Some("google-a.json") {
                    return "unexpected retry".into_response();
                }
                let body = futures_util::stream::once(async move {
                    release.notified().await;
                    Err::<Bytes, _>(std::io::Error::other("upstream disconnected"))
                });
                (StatusCode::FORBIDDEN, [("x-request-id", "forbidden")], Body::from_stream(body)).into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 1;
    let server = serve(proxy.clone().app()).await;
    let mut response = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.headers()["x-request-id"], "forbidden");
    release.notify_one();
    assert!(response.chunk().await.is_err());
    let state = proxy.state.read().await;
    let attempts = &state.activity.back().unwrap().attempts;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status_code, Some(403));
}

#[tokio::test]
async fn google_in_band_auth_exhaustion_uses_the_same_project_reset_on_repeat() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|| async {
            axum::Json(json!([
                {"usageMetadata":{"cachedContentTokenCount":20,"totalTokenCount":110}},
                {"usageMetadata":{"promptTokenCount":100}},
                {"error":{"code":401,"status":"UNAUTHENTICATED","message":"fixture"}}
            ]))
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let expected = DateTime::from_timestamp(Utc::now().timestamp() + 120, 0).unwrap();
    {
        let mut state = proxy.state.write().await;
        for id in ["google-a.json", "google-b.json"] {
            state.pool.accounts.get_mut(id).unwrap().project = Some("alpha".into());
        }
        let b = state.pool.accounts.get_mut("google-b.json").unwrap();
        b.cooldown = Some(pool::Cooldown { until: expected, verified_reset: true });
        let mut other = b.clone();
        other.id = "other-project.json".into();
        other.project = Some("beta".into());
        other.cooldown = Some(pool::Cooldown { until: Utc::now() + TimeDelta::seconds(30), verified_reset: true });
        state.pool.accounts.insert(other.id.clone(), other);
    }
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    for first in [true, false] {
        let res = http
            .request("POST", "/v1internal:generateContent")
            .bearer_auth("inference-fixture")
            .json(&json!({"model":"gemini-4-neon-high","cachedContent":"projects/alpha/cachedContents/cache"}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 429);
        let retry: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((115..=120).contains(&retry), "{retry}");
        // A cached request cannot witness the shorter reset of another project.
        assert_eq!(google_witness(res.headers(), "gemini-4-neon-high"), Some(expected));
        assert_google_pool_body(&res.json::<Value>().await.unwrap(), "gemini-4-neon-high");
        let state = proxy.state.read().await;
        let attempts = &state.activity.back().unwrap().attempts;
        if first {
            assert_eq!(attempts.len(), 1);
            let attempt = &attempts[0];
            assert_eq!(attempt.status_code, Some(401));
            assert_eq!((attempt.input_tokens, attempt.output_tokens, attempt.cache_read_tokens), (100, 10, 20));
        } else {
            assert!(attempts.is_empty());
        }
    }
}

#[tokio::test]
async fn header_cooldown_fails_over_without_waiting_for_a_stalled_body() {
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post(|headers: HeaderMap| async move {
            if router::bearer(&headers) == Some("google-a.json") {
                let body = Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
                (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "30")], body).into_response()
            } else {
                "alternate".into_response()
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    proxy.state.write().await.pool.policy.first_byte_timeout_secs = 30;
    let server = serve(proxy.app()).await;
    let response = Http::new(&server)
        .request("POST", "/v1internal:generateContent")
        .bearer_auth("inference-fixture")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "alternate");
}

#[tokio::test]
async fn a_passthrough_429_reports_a_broken_stream_after_its_prefix() {
    let source = Arc::new(std::sync::OnceLock::<Arc<Proxy>>::new());
    let mock = serve(Router::new().route(
        "/v1internal:generateContent",
        post({
            let source = source.clone();
            move || {
                let source = source.clone();
                async move {
                    source.get().unwrap().state.write().await.pool.accounts.clear();
                    let lead = futures_util::stream::once(async { Ok(Bytes::from_static(b"prefix")) });
                    let broken = futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Err::<Bytes, _>(std::io::Error::other("upstream disconnected"))
                    });
                    (StatusCode::TOO_MANY_REQUESTS, Body::from_stream(futures_util::StreamExt::chain(lead, broken)))
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    assert!(source.set(proxy.clone()).is_ok());
    let server = serve(proxy.clone().app()).await;
    let mut response =
        Http::new(&server).request("POST", "/v1internal:generateContent").bearer_auth("inference-fixture").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.chunk().await.unwrap().unwrap(), Bytes::from_static(b"prefix"));
    assert!(response.chunk().await.is_err());
    assert_eq!(proxy.state.read().await.activity.back().unwrap().outcome, "stream_error");
}

#[tokio::test]
async fn google_usage_probe_renews_then_retains_rejected_buckets_only_on_the_same_route() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
    let usable = Arc::new(AtomicBool::new(true));
    let probes = Arc::new(AtomicUsize::new(0));
    let mock = serve(
        Router::new()
            .route("/test/google-token", post(|| async { axum::Json(json!({"access_token":"google-fresh","expires_in":3600})) }))
            .route(
                "/v1internal:loadCodeAssist",
                post(|headers: HeaderMap| async move {
                    if router::bearer(&headers) == Some("google-fresh") {
                        axum::Json(json!({"cloudaicompanionProject":"fresh-project"})).into_response()
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }),
            )
            .route(
                "/v1internal:retrieveUserQuotaSummary",
                post({
                    let probes = probes.clone();
                    move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                        let probes = probes.clone();
                        async move {
                            probes.fetch_add(1, Relaxed);
                            if router::bearer(&headers) != Some("google-fresh") {
                                assert_eq!(body, json!({}));
                                StatusCode::UNAUTHORIZED.into_response()
                            } else {
                                assert_eq!(body, json!({"project":"fresh-project"}));
                                axum::Json(json!({"buckets":[{"bucketId":"gemini-5h","window":"5h"}]})).into_response()
                            }
                        }
                    }
                }),
            )
            .route(
                "/v1internal:fetchAvailableModels",
                post({
                    let usable = usable.clone();
                    move |headers: HeaderMap| {
                        let usable = usable.clone();
                        async move {
                            assert_eq!(router::bearer(&headers), Some("google-fresh"));
                            let remaining = usable.load(Relaxed).then_some(0.995);
                            axum::Json(json!({"models":{"gemini-4-argon-high":{"quotaInfo":{"remainingFraction":remaining}}}}))
                        }
                    }
                }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let mut account = pool::parse_credentials(
        "google.json",
        &json!({"type":"google","access_token":"google-old","refresh_token":"refresh","client_id":"client",
            "client_secret":"secret","email":"fixture@example.test"}),
        false,
    )
    .unwrap()
    .remove(0);
    let at = Utc::now();
    account.windows = integrations::providers::antigravity::parse_antigravity_summary(
        &json!({"buckets":[
            {"bucketId":"gemini-5h","window":"5h","remainingFraction":0,"resetTime":at + TimeDelta::hours(4)},
            {"bucketId":"3p-5h","window":"5h","remainingFraction":0,"resetTime":at - TimeDelta::hours(1)},
        ]}),
        at,
    );
    account.windows.iter_mut().for_each(|w| w.status = QuotaStatus::Rejected);
    proxy.state.write().await.pool.insert(vec![account.clone()]);
    proxy.refresh_usage(&account).await.unwrap();
    assert_eq!(probes.load(Relaxed), 2);
    {
        let state = proxy.state.read().await;
        let next = &state.pool.accounts["google.json"];
        assert!(!next.probe_failed);
        assert_eq!(next.access, "google-fresh");
        assert_eq!(next.project.as_deref(), Some("fresh-project"));
        assert_eq!(next.windows.iter().map(|w| w.window_key.as_str()).collect::<Vec<_>>(), ["gemini-argon-quota"]);
        assert!(next.eligible(Utc::now(), "gemini-4-neon-high"));
    }
    let current = {
        let mut state = proxy.state.write().await;
        let next = state.pool.accounts.get_mut("google.json").unwrap();
        // A blocker observed for the newly enrolled route survives a partial probe of that route.
        next.windows.extend(account.windows.clone());
        next.clone()
    };
    proxy.refresh_usage(&current).await.unwrap();
    {
        let state = proxy.state.read().await;
        let next = &state.pool.accounts["google.json"];
        assert_eq!(next.windows.iter().map(|w| w.window_key.as_str()).collect::<Vec<_>>(), ["gemini-5h", "gemini-argon-quota"]);
        assert!(!next.eligible(Utc::now(), "gemini-4-neon-high"));
        assert!(next.eligible(Utc::now(), "gemini-4-argon-high"));
    }
    usable.store(false, Relaxed);
    assert_eq!(proxy.refresh_usage(&account).await, Err("credential changed before quota refresh"));
    let current = {
        let mut state = proxy.state.write().await;
        let next = state.pool.accounts.get_mut("google.json").unwrap();
        next.windows.iter_mut().find(|w| w.window_key == "gemini-argon-quota").unwrap().observed_at = Some(at - TimeDelta::hours(2));
        next.clone()
    };
    assert_eq!(proxy.refresh_usage(&current).await, Err("provider returned no quota evidence"));
    let state = proxy.state.read().await;
    let next = &state.pool.accounts["google.json"];
    assert!(next.probe_failed);
    assert_eq!(next.windows.len(), 1);
    assert_eq!(next.windows[0].window_key, "gemini-5h");
    assert!(next.windows.iter().all(|w| w.stale));
}

#[tokio::test]
async fn google_summary_retires_only_superseded_fallbacks_and_manual_reset_clears_backoff() {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    let summary_ready = Arc::new(AtomicBool::new(false));
    let now = Utc::now();
    let mock = serve(
        Router::new()
            .route(
                "/v1internal:retrieveUserQuotaSummary",
                post({
                    let summary_ready = summary_ready.clone();
                    move || {
                        let ready = summary_ready.load(Relaxed);
                        async move {
                            let buckets = if ready { json!([
                                {"bucketId":"gemini-5h","window":"5h","remainingFraction":1.0},
                                {"bucketId":"gemini-argon-5h","window":"5h","remainingFraction":1.0},
                            ]) } else { json!([]) };
                            axum::Json(json!({"buckets":buckets}))
                        }
                    }
                }),
            )
            .route(
                "/v1internal:fetchAvailableModels",
                post(move || async move {
                    axum::Json(json!({"models":{
                        "gemini-neon":{"quotaInfo":{"remainingFraction":0},"weeklyQuotaInfo":{"remainingFraction":0,"resetTime":now + TimeDelta::days(1)}},
                        "gpt-5":{"quotaInfo":{"remainingFraction":0}},
                    }}))
                }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let mut account = proxy.state.read().await.pool.accounts["google-a.json"].clone();
    proxy.refresh_usage(&account).await.unwrap();
    {
        let mut state = proxy.state.write().await;
        let current = state.pool.accounts.get_mut(&account.id).unwrap();
        assert!(!current.eligible(now, "gemini-neon"));
        current.windows.extend(integrations::providers::antigravity::parse_antigravity_summary(
            &json!({"buckets":[
                {"bucketId":"gemini-argon-weekly","window":"weekly","remainingFraction":0,"resetTime":now + TimeDelta::days(1)},
                {"bucketId":"gemini-argon-obsolete","remainingFraction":0},
                {"bucketId":"obsolete","remainingFraction":0},
            ]}),
            now - TimeDelta::hours(1) - TimeDelta::seconds(1),
        ));
        // The stale undated window no longer enforces routing even before the next probe.
        assert!(current.eligible(now, "obsolete"));
        current.windows.iter_mut().find(|w| w.window_key == "gemini-argon-obsolete").unwrap().observed_at = Some(now);
    }
    summary_ready.store(true, Relaxed);
    proxy.refresh_usage(&account).await.unwrap();
    {
        let mut state = proxy.state.write().await;
        let current = state.pool.accounts.get_mut(&account.id).unwrap();
        let keys = current.windows.iter().map(|w| w.window_key.as_str()).collect::<Vec<_>>();
        // A healthy 5-hour summary retires the non-weekly fallback (`gemini-quota`) without clearing
        // an active weekly exhaustion (`gemini-weekly`) until weekly recovery or manual reset.
        assert_eq!(keys, ["3p-quota", "gemini-5h", "gemini-argon-5h", "gemini-argon-weekly", "gemini-weekly"]);
        assert!(!current.eligible(Utc::now(), "gemini-neon"));
        assert!(!current.eligible(Utc::now(), "gemini-argon"));
        assert!(!current.eligible(Utc::now(), "claude-opus-4"));
        // Undated Google exhaustion advertises its 1-hour expiry deadline, not MAX_UTC.
        let claude_limit = current.quota_limited_until(now, "claude-opus-4").unwrap();
        assert!(claude_limit <= now + TimeDelta::hours(1) + TimeDelta::seconds(5));
        current.auth_cooldown = Some(now + TimeDelta::minutes(1));
        account = current.clone();
    }
    let server = serve(proxy.clone().app()).await;
    let reset: Value = Http::new(&server)
        .management("POST", "/v0/management/reset-quota", Some(json!({"auth_index":account.auth_index})))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(reset["windows_reset"], 5);
    let state = proxy.state.read().await;
    let current = &state.pool.accounts[&account.id];
    assert!(current.eligible(Utc::now(), "gemini-neon"));
    assert!(current.auth_cooldown.is_none());
    assert!(current.eligible(Utc::now(), "claude-opus-4"));
}

#[tokio::test]
async fn google_probe_rechecks_the_enrolled_route_before_merging() {
    let began = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:retrieveUserQuotaSummary",
        post({
            let (began, release) = (began.clone(), release.clone());
            move || {
                let (began, release) = (began.clone(), release.clone());
                async move {
                    began.notify_one();
                    release.notified().await;
                    axum::Json(json!({"buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0.0}]}))
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    for changed in ["project", "base", "access"] {
        let accounts = pool::parse_credentials(
            "google.json",
            &json!({"type":"google","access_token":"fixture","project_id":"original","email":"person@example.test"}),
            false,
        )
        .unwrap();
        let account = accounts[0].clone();
        proxy.state.write().await.pool.insert(accounts);
        let replace_route = async {
            began.notified().await;
            let mut state = proxy.state.write().await;
            let current = state.pool.accounts.get_mut(&account.id).unwrap();
            // Leave the generation unchanged to exercise the independent route check.
            match changed {
                "project" => current.project = Some("replacement".into()),
                "base" => current.base = Some("https://replacement.example.test".parse().unwrap()),
                _ => current.access = "replacement".into(),
            }
            release.notify_one();
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(proxy.refresh_usage(&account), replace_route) })
                .await
                .unwrap();
        assert_eq!(result, Err("quota refresh was superseded"), "{changed}");
        assert!(proxy.state.read().await.pool.accounts[&account.id].windows.is_empty());
    }
}

#[tokio::test]
async fn late_google_probe_cannot_overwrite_a_newer_healthy_probe() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let visits = Arc::new(AtomicUsize::new(0));
    let began = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mock = serve(Router::new().route(
        "/v1internal:retrieveUserQuotaSummary",
        post({
            let (began, release) = (began.clone(), release.clone());
            move || {
                let (began, release) = (began.clone(), release.clone());
                let stale = visits.fetch_add(1, Relaxed) == 0;
                async move {
                    if stale {
                        began.notify_one();
                        release.notified().await;
                    }
                    let remaining = if stale { 0 } else { 1 };
                    axum::Json(json!({"buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":remaining}]}))
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let proxy = google_routing_fixture(&dir, &mock).await;
    let account = proxy.state.read().await.pool.accounts["google-a.json"].clone();
    let newer = async {
        began.notified().await;
        let healthy = proxy.refresh_usage(&account).await;
        release.notify_one();
        healthy
    };
    let (older, newer) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(proxy.refresh_usage(&account), newer) }).await.unwrap();
    assert_eq!(older, Err("quota refresh was superseded"));
    newer.unwrap();
    let state = proxy.state.read().await;
    let current = &state.pool.accounts[&account.id];
    assert_eq!(current.probe_generation, 2);
    assert_eq!(current.windows[0].used_percent, Some(0.0));
    assert!(current.eligible(Utc::now(), "gemini-neon"));
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
        exclude_local_lanes: Default::default(),
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

#[tokio::test]
async fn provenance_oauth_reloads_new_enrollment_on_google_auth_miss_and_throttles() {
    let dir = Scratch::new();
    let oauth_dir = dir.join("provenance-oauth");
    config::private_dir(&oauth_dir).unwrap();
    let user = hub_sync::source_identity().1;

    config::write_private(
        &oauth_dir.join(format!("{user}-google.json")),
        &serde_json::to_vec(&json!({"target":{"provider":"google","profile":"antigravity","user":user}})).unwrap(),
        true,
    )
    .unwrap();

    let calls_file = dir.join("provenance-calls");
    let rotated_flag = dir.join("rotated-google");
    let fake_bin = dir.join("fake-provenance-oauth");
    fs::write(
        &fake_bin,
        format!(
            "#!/bin/sh\n\
             printf 'x\\n' >> {}\n\
             case \"$2\" in\n\
               *-google.json) if [ -f {} ]; then printf '{{\"client_id\":\"agy-client\",\"client_secret\":\"agy-secret\",\"project_id\":\"aicode-consumers\",\"refresh_token\":\"refresh-1-rotated\"}}'; else printf '{{\"client_id\":\"agy-client\",\"client_secret\":\"agy-secret\",\"project_id\":\"aicode-consumers\",\"refresh_token\":\"refresh-1\"}}'; fi ;;\n\
               *-google-new.json) printf '{{\"client_id\":\"agy-client\",\"client_secret\":\"agy-secret\",\"project_id\":\"aicode-consumers\",\"refresh_token\":\"refresh-new\"}}' ;;\n\
               *-google-third.json) printf '{{\"client_id\":\"agy-client\",\"client_secret\":\"agy-secret\",\"project_id\":\"aicode-consumers\",\"refresh_token\":\"refresh-third\"}}' ;;\n\
               *) exit 1 ;;\n\
             esac\n",
            calls_file.display(),
            rotated_flag.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o700)).unwrap();

    let mock = serve(
        Router::new()
            .route(
                "/test/google-token",
                post(|body: String| async move {
                    let token = if body.contains("refresh_token=refresh-new") {
                        "ya29.pool-new"
                    } else if body.contains("refresh_token=refresh-third") {
                        "ya29.pool-third"
                    } else {
                        "ya29.pool-1"
                    };
                    axum::Json(json!({"access_token": token, "expires_in": 3600})).into_response()
                }),
            )
            .route(
                "/test/google-tokeninfo",
                post(|body: String| async move {
                    match body.as_str() {
                        "access_token=ya29.pool-1" => axum::Json(json!({
                            "aud": "agy-client",
                            "email": "one@example.test",
                            "email_verified": "true",
                            "expires_in": "3599"
                        }))
                        .into_response(),
                        "access_token=ya29.client-new" | "access_token=ya29.pool-new" => axum::Json(json!({
                            "aud": "agy-client",
                            "email": "new@example.test",
                            "email_verified": "true",
                            "expires_in": "3599"
                        }))
                        .into_response(),
                        "access_token=ya29.client-unknown" => axum::Json(json!({
                            "aud": "agy-client",
                            "email": "unknown@example.test",
                            "email_verified": "true",
                            "expires_in": "3599"
                        }))
                        .into_response(),
                        _ => StatusCode::BAD_REQUEST.into_response(),
                    }
                }),
            ),
    )
    .await;

    let config = Config {
        bind: config::DEFAULT_BIND.parse().unwrap(),
        home: dir.0.clone(),
        t3code_home: dir.0.clone(),
        auth_dir: dir.join("auth"),
        discover_local: true,
        exclude_local_lanes: Default::default(),
        provenance_oauth_dir: oauth_dir.clone(),
        provenance_oauth_bin: Some(fake_bin),
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
    let proxy = Arc::new(proxy);
    async fn google_count(proxy: &Proxy) -> usize {
        proxy.state.read().await.pool.accounts.values().filter(|a| a.lane == Lane::Google).count()
    }
    proxy.discover_local_commands().await;
    assert_eq!(google_count(&proxy).await, 1);
    let rev_before = proxy.state.read().await.pool.revision;

    // A new enrollment arrives after startup; concurrent authorize_google callers both succeed.
    config::write_private(
        &oauth_dir.join(format!("{user}-google-new.json")),
        &serde_json::to_vec(&json!({"target":{"provider":"google","profile":"antigravity","user":user}})).unwrap(),
        true,
    )
    .unwrap();
    let (first_ok, second_ok) = tokio::join!(proxy.authorize_google("ya29.client-new"), proxy.authorize_google("ya29.client-new"));
    assert!(first_ok && second_ok);
    assert_eq!(google_count(&proxy).await, 2);
    assert!(proxy.state.read().await.pool.revision > rev_before);

    // An immediate follow-up miss is throttled and does not re-spawn provenance-oauth.
    let calls_after_reload = fs::read_to_string(&calls_file).unwrap().lines().count();
    assert!(!proxy.authorize_google("ya29.client-unknown").await);
    assert_eq!(fs::read_to_string(&calls_file).unwrap().lines().count(), calls_after_reload);

    // Rotating a discovered account's refresh token preserves operator overrides (disabled/priority/weight/note).
    let google_id = {
        let mut state = proxy.state.write().await;
        let account = state.pool.accounts.values_mut().find(|a| a.lane == Lane::Google && a.label == "google").unwrap();
        account.disabled = true;
        account.priority = 7;
        account.weight = 3;
        account.note = Some("paused by operator".into());
        account.id.clone()
    };
    let rev_before_rotation = proxy.state.read().await.pool.revision;
    fs::write(&rotated_flag, "1").unwrap();
    // Explicit unthrottled refresh picks up the rotated token and another newly deployed manifest.
    config::write_private(
        &oauth_dir.join(format!("{user}-google-third.json")),
        &serde_json::to_vec(&json!({"target":{"provider":"google","profile":"antigravity","user":user}})).unwrap(),
        true,
    )
    .unwrap();
    assert!(proxy.refresh_local_commands(Duration::ZERO).await);
    assert_eq!(google_count(&proxy).await, 3);
    let state = proxy.state.read().await;
    assert!(state.pool.revision > rev_before_rotation);
    let rotated = state.pool.accounts.get(&google_id).unwrap();
    assert_eq!(rotated.refresh.as_deref(), Some("refresh-1-rotated"));
    assert!(rotated.disabled);
    assert_eq!(rotated.priority, 7);
    assert_eq!(rotated.weight, 3);
    assert_eq!(rotated.note.as_deref(), Some("paused by operator"));
}

#[tokio::test]
async fn expired_claude_token_reloads_from_disk_and_activity_preserves_effort() {
    let seen_auth = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured = seen_auth.clone();
    let mock = serve(
        Router::new()
            .route(
                "/v1/messages",
                post(move |headers: HeaderMap| {
                    let captured = captured.clone();
                    async move {
                        if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
                            captured.lock().await.push(auth.to_owned());
                        }
                        axum::Json(json!({"usage":{"input_tokens":10,"output_tokens":5}}))
                    }
                }),
            )
            .route(
                "/api/oauth/usage",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer reloaded-usage-token");
                    axum::Json(json!({"five_hour":{"utilization":25,"resets_at": Utc::now() + TimeDelta::hours(4)}}))
                }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let cred_path = dir.0.join("auth/a.json");
    // Mark a.json expired in memory, and write a fresher token to disk.
    {
        let mut state = proxy.state.write().await;
        if let Some(a) = state.pool.accounts.get_mut("a.json") {
            a.expires = Some(Utc::now() - TimeDelta::minutes(5));
        }
        if let Some(b) = state.pool.accounts.get_mut("b.json") {
            b.disabled = true;
        }
    }
    fs::write(
        &cred_path,
        json!({
            "type": "claude",
            "access_token": "reloaded-fresh-token",
            "expires_at": (Utc::now() + TimeDelta::hours(1)).to_rfc3339()
        })
        .to_string(),
    )
    .unwrap();
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let response = http
        .request("POST", "/v1/messages")
        .bearer_auth("inference-fixture")
        .json(&json!({"model":"claude-fixture","reasoning":{"effort":"high"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = response.bytes().await.unwrap();
    assert_eq!(seen_auth.lock().await.as_slice(), ["Bearer reloaded-fresh-token"]);
    {
        let state = proxy.state.read().await;
        assert_eq!(state.pool.accounts["a.json"].success, 1, "reloaded account must record request success");
        let recorded = state.activity.back().expect("exchange must be recorded");
        assert_eq!(recorded.effort.as_deref(), Some("high"));
        assert_eq!(recorded.attempts[0].effort.as_deref(), Some("high"));
    }
    // Expire again and rotate both access and refresh tokens on disk; refresh_usage must succeed
    // and update windows instead of rejecting with "credential changed during quota refresh".
    let snapshot = {
        let mut state = proxy.state.write().await;
        let a = state.pool.accounts.get_mut("a.json").unwrap();
        a.expires = Some(Utc::now() - TimeDelta::minutes(5));
        a.refresh = Some("old-refresh".into());
        a.clone()
    };
    fs::write(
        &cred_path,
        json!({
            "type": "claude",
            "access_token": "reloaded-usage-token",
            "refresh_token": "new-refresh",
            "expires_at": (Utc::now() + TimeDelta::hours(1)).to_rfc3339()
        })
        .to_string(),
    )
    .unwrap();
    proxy.refresh_usage(&snapshot).await.expect("usage refresh should accept reloaded rotated credentials");
    let state = proxy.state.read().await;
    assert_eq!(state.pool.accounts["a.json"].windows.len(), 1);
    assert_eq!(state.pool.accounts["a.json"].windows[0].used_percent, Some(25.0));
}

#[tokio::test]
async fn google_project_discovery_hydrates_uploads_and_supplies_quota_requests() {
    let mock = serve(
        Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                post(|headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(headers["user-agent"], "antigravity/hub/2.24.0");
                    assert_eq!(body, json!({"metadata":{"ideType":"ANTIGRAVITY"}}));
                    let project = match router::bearer(&headers) {
                        Some("google-upload") => json!(" upload-project "),
                        Some("google-probe") => json!({"id":" probe-project "}),
                        _ => panic!("unexpected Google access token"),
                    };
                    axum::Json(json!({"cloudaicompanionProject": project}))
                }),
            )
            .route("/test/google-tokeninfo", post(|| async { axum::Json(json!({"email_verified":true,"email":"google@example.test"})) }))
            .route(
                "/v1internal:retrieveUserQuotaSummary",
                post(|axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(body, json!({"project":"probe-project"}));
                    axum::Json(json!({"buckets":[{"bucketId":"gemini-5h","window":"5h","remainingFraction":0.75}]}))
                }),
            ),
    )
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let inventory = "/v0/management/auth-files";
    let upload = http
        .management("POST", "/v8/management/credentials?name=upload.json", Some(json!({"type":"google","access_token":"google-upload"})))
        .await;
    assert_eq!(upload.status(), StatusCode::OK);
    let uploaded = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows: Value = http.management("GET", inventory, None).await.json().await.unwrap();
            if let Some(row) =
                rows["files"].as_array().unwrap().iter().find(|row| row["id"] == "upload.json" && row["project_id"] == "upload-project")
            {
                break row.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("uploaded project ID should appear in inventory");
    assert_eq!(uploaded["email"], "google@example.test");
    assert_eq!(uploaded["status"], "active");

    let mut account = pool::parse_credentials(
        "probe.json",
        &json!({"type":"google","access_token":"google-probe","email":"existing@example.test"}),
        false,
    )
    .unwrap()
    .remove(0);
    seed_old_google_route(&mut account);
    let revision = {
        let mut state = proxy.state.write().await;
        state.pool.insert(vec![account.clone()]);
        state.pool.revision
    };
    proxy.refresh_usage(&account).await.unwrap();
    {
        let state = proxy.state.read().await;
        let current = &state.pool.accounts["probe.json"];
        assert_eq!(state.pool.revision, revision + 1);
        assert!(current.eligible(Utc::now(), "gemini-neon"));
        assert!(current.cooldown.is_none());
        assert!(current.scoped_cooldowns.is_empty());
        assert!(current.auth_cooldown.is_none());
        assert_eq!(current.windows.len(), 1);
        assert_eq!(current.windows[0].used_percent, Some(25.0));
    }
    let rows: Value = http.management("GET", inventory, None).await.json().await.unwrap();
    let probe = rows["files"].as_array().unwrap().iter().find(|row| row["id"] == "probe.json").unwrap();
    assert_eq!(probe["project_id"], "probe-project");
}

#[test]
fn provenance_reload_replaces_the_enrolled_google_project() {
    let mut pool = Pool::default();
    let enrollment = |project, refresh| json!({"access_token":"fixture","refresh_token":refresh,"project_id":project});
    pool::add_provenance_google(&mut pool, "enrollment", enrollment("project-a", "old-refresh"));
    let old = pool.accounts.values_mut().next().unwrap();
    let id = old.id.clone();
    seed_old_google_route(old);
    let old_generation = old.probe_generation;
    pool::add_provenance_google(&mut pool, "enrollment", enrollment("project-b", "old-refresh"));
    let account = &pool.accounts[&id];
    assert_eq!(account.project.as_deref(), Some("project-b"));
    assert!(account.probe_generation > old_generation);
    assert_google_route_cleared(account);

    pool::add_provenance_google(&mut pool, "enrollment", enrollment("project-c", "new-refresh"));
    assert_eq!(pool.accounts.len(), 1);
    assert_eq!(pool.accounts[&id].project.as_deref(), Some("project-c"));
    assert_eq!(pool.accounts[&id].refresh.as_deref(), Some("new-refresh"));
}

#[test]
fn provenance_reload_replaces_an_access_token_without_a_refresh_token() {
    let mut pool = Pool::default();
    let enrollment = |access| json!({"access_token":access,"project_id":"project"});
    pool::add_provenance_google(&mut pool, "enrollment", enrollment("old-token"));
    let old = pool.accounts.values_mut().next().unwrap();
    let id = old.id.clone();
    seed_old_google_route(old);
    let old_generation = old.probe_generation;

    pool::add_provenance_google(&mut pool, "enrollment", enrollment("old-token"));
    assert_eq!(pool.accounts[&id].probe_generation, old_generation);
    assert!(pool.accounts[&id].cooldown.is_some());

    pool::add_provenance_google(&mut pool, "enrollment", enrollment("new-token"));
    let account = &pool.accounts[&id];
    assert_eq!(pool.accounts.len(), 1);
    assert_eq!(account.access, "new-token");
    assert!(account.refresh.is_none());
    assert!(account.probe_generation > old_generation);
    assert_google_route_cleared(account);
}

#[test]
fn provenance_reload_deduplicates_access_token_without_clearing_cooldown() {
    let mut pool = Pool::default();
    let enrollment = |access| json!({"access_token":access,"project_id":"project"});
    pool::add_provenance_google(&mut pool, "a", enrollment("token-a"));
    pool::add_provenance_google(&mut pool, "b", enrollment("token-b"));
    assert_eq!(pool.accounts.len(), 2);
    let b = pool.accounts.values_mut().find(|a| a.label == "b").unwrap();
    seed_old_google_route(b);
    let b_id = b.id.clone();
    let cooldown_until = b.cooldown.unwrap().until;
    let generation = b.probe_generation;
    let revision = pool.revision;

    pool::add_provenance_google(&mut pool, "a", enrollment("token-b"));

    assert_eq!(pool.accounts.len(), 1);
    assert!(pool.revision > revision);
    let b = &pool.accounts[&b_id];
    assert_eq!(b.label, "b");
    assert_eq!(b.access, "token-b");
    assert_eq!(b.cooldown.unwrap().until, cooldown_until);
    assert_eq!(b.probe_generation, generation);
    assert!(!b.eligible(Utc::now(), "gemini-neon"));
}

#[test]
fn provenance_reload_keeps_the_refresh_token_and_surviving_route_evidence() {
    for (offset, client, secret) in [
        (-600, Some("new-client"), Some("new-secret")),
        (0, Some("new-client"), Some("new-secret")),
        (600, Some("new-client"), None),
        (600, None, Some("new-secret")),
        (600, None, None),
    ] {
        let mut pool = Pool::default();
        let expires = Utc::now().timestamp() + 3600;
        pool::add_provenance_google(&mut pool, "a", json!({"access_token":"old-access","refresh_token":"old-refresh"}));
        let access_only = json!({
            "access_token":"shared-access","expires_at":expires + offset,
            "client_id":"old-client","client_secret":"old-secret","project_id":"project"
        });
        pool::add_provenance_google(&mut pool, "b", access_only.clone());
        assert_eq!(pool.accounts.len(), 2);
        let b = pool.accounts.values_mut().find(|a| a.label == "b").unwrap();
        assert!(b.refresh.is_none());
        seed_old_google_route(b);
        let b_id = b.id.clone();
        let (cooldown, auth, scoped, reset, generation) =
            (b.cooldown, b.auth_cooldown, b.scoped_cooldowns.clone(), b.windows[0].resets_at, b.probe_generation);

        pool::add_provenance_google(
            &mut pool,
            "a",
            json!({
                "access_token":"shared-access","refresh_token":"new-refresh","expires_at":expires,
                "client_id":client,"client_secret":secret,"project_id":"project"
            }),
        );
        // Discovery may reread the survivor's access-only enrollment in the same pass.
        pool::add_provenance_google(&mut pool, "b", access_only);

        assert_eq!(pool.accounts.len(), 1);
        let b = &pool.accounts[&b_id];
        assert_eq!(b.refresh.as_deref(), Some("new-refresh"));
        assert_eq!(b.expires.map(|at| at.timestamp()), Some(expires.max(expires + offset)));
        assert_eq!(b.google_client, client.map(str::to_owned).or_else(|| std::env::var("PROXY_GOOGLE_CLIENT_ID").ok()));
        assert_eq!(b.google_secret, secret.map(str::to_owned).or_else(|| std::env::var("PROXY_GOOGLE_CLIENT_SECRET").ok()));
        assert_eq!(b.cooldown, cooldown);
        assert_eq!(b.auth_cooldown, auth);
        assert_eq!(b.scoped_cooldowns, scoped);
        assert_eq!(b.windows[0].resets_at, reset);
        assert_eq!(b.probe_generation, generation);
        assert!(!b.eligible(Utc::now(), "gemini-neon"));
    }
}

#[test]
fn provenance_reload_does_not_mix_oauth_clients_across_refresh_tokens() {
    let mut pool = Pool::default();
    pool::add_provenance_google(&mut pool, "a", json!({"access_token":"old-access"}));
    pool::add_provenance_google(&mut pool, "b", json!({"access_token":"shared-access"}));
    let b_id = pool.accounts.values().find(|a| a.label == "b").unwrap().id.clone();
    pool::add_provenance_google(&mut pool, "a", json!({"access_token":"shared-access","refresh_token":"adopted-refresh"}));
    assert_eq!(pool.accounts.len(), 1);
    let b = pool.accounts.get_mut(&b_id).unwrap();
    assert_eq!(b.refresh.as_deref(), Some("adopted-refresh"));
    // Keep this fixture independent of ambient Google OAuth defaults.
    b.google_client = None;
    b.google_secret = None;

    for refresh in [None, Some("unrelated-refresh")] {
        pool::add_provenance_google(&mut pool, "a", json!({"access_token":"other-access"}));
        assert_eq!(pool.accounts.len(), 2);
        pool::add_provenance_google(
            &mut pool,
            "a",
            json!({"access_token":"shared-access","refresh_token":refresh,"client_id":"wrong-client","client_secret":"wrong-secret"}),
        );
        assert_eq!(pool.accounts.len(), 1);
        let b = &pool.accounts[&b_id];
        assert_eq!(b.refresh.as_deref(), Some("adopted-refresh"));
        assert_eq!(b.google_client, None);
        assert_eq!(b.google_secret, None);
    }
}

#[test]
fn provenance_reload_deduplicates_refresh_token_and_enriches_the_retained_route() {
    let mut pool = Pool::default();
    let enrollment = |access, refresh| json!({"access_token":access,"refresh_token":refresh});
    pool::add_provenance_google(&mut pool, "a", enrollment("token-a", "refresh-a"));
    pool::add_provenance_google(&mut pool, "b", enrollment("token-b", "refresh-b"));
    assert_eq!(pool.accounts.len(), 2);
    let b = pool.accounts.values_mut().find(|a| a.label == "b").unwrap();
    seed_old_google_route(b);
    let b_id = b.id.clone();
    let generation = b.probe_generation;

    let mut updated = enrollment("new-token", "refresh-b");
    updated["project_id"] = json!("project");
    pool::add_provenance_google(&mut pool, "a", updated);

    assert_eq!(pool.accounts.len(), 1);
    let b = &pool.accounts[&b_id];
    assert_eq!(b.access, "token-b");
    assert_eq!(b.project.as_deref(), Some("project"));
    assert!(b.probe_generation > generation);
    assert_google_route_cleared(b);
}

#[test]
fn provenance_reload_updates_inherited_but_not_explicit_uploaded_projects() {
    let mut pool = Pool::default();
    let enrollment = |refresh, project| json!({"refresh_token":refresh,"project_id":project});
    for (id, refresh, project) in [("inherited.json", "inherited-refresh", None), ("explicit.json", "explicit-refresh", Some("project-a"))]
    {
        let mut raw = enrollment(refresh, project);
        raw["type"] = json!("google");
        raw["access_token"] = json!("uploaded-token");
        pool.insert(pool::parse_credentials(id, &raw, false).unwrap());
        pool::add_provenance_google(&mut pool, id, enrollment(refresh, Some("project-a")));
        assert_eq!(pool.accounts[id].project.as_deref(), Some("project-a"));
        let old = pool.accounts.get_mut(id).unwrap();
        seed_old_google_route(old);
        let old_generation = old.probe_generation;

        pool::add_provenance_google(&mut pool, id, enrollment(refresh, Some("project-b")));
        let account = &pool.accounts[id];
        assert!(!account.local);
        assert_eq!(account.access, "uploaded-token");
        if project.is_none() {
            assert_eq!(account.project.as_deref(), Some("project-b"));
            assert!(account.probe_generation > old_generation);
            assert_google_route_cleared(account);
        } else {
            assert_eq!(account.project.as_deref(), project);
            assert_eq!(account.probe_generation, old_generation);
            assert!(account.cooldown.is_some());
        }
    }
    assert_eq!(pool.accounts.len(), 2);
}

fn seed_old_google_route(account: &mut Account) {
    let now = Utc::now();
    let until = now + TimeDelta::minutes(10);
    let cooldown = pool::Cooldown { until, verified_reset: true };
    account.cooldown = Some(cooldown);
    account.scoped_cooldowns.insert("gemini".into(), cooldown);
    account.auth_cooldown = Some(until);
    account.windows = integrations::providers::antigravity::parse_antigravity_summary(
        &json!({"buckets":[{"bucketId":"gemini-5h","remainingFraction":0.0,"resetTime":until}]}),
        now,
    );
    account.google_fallback_keys.insert(account.windows[0].window_key.clone());
    account.observed = Some(now);
}

fn assert_google_route_cleared(account: &Account) {
    assert!(account.cooldown.is_none());
    assert!(account.scoped_cooldowns.is_empty());
    assert!(account.auth_cooldown.is_none());
    assert!(account.windows.is_empty());
    assert!(account.google_fallback_keys.is_empty());
    assert!(account.observed.is_none());
}

#[tokio::test]
async fn google_hydration_discards_evidence_from_the_projectless_route() {
    let mock = serve(
        Router::new().route("/v1internal:loadCodeAssist", post(|| async { axum::Json(json!({"cloudaicompanionProject":"discovered"})) })),
    )
    .await;
    let dir = Scratch::new();
    let proxy = fixture(&dir, &mock);
    let mut accounts =
        pool::parse_credentials("google.json", &json!({"type":"google","access_token":"fixture","email":"person@example.test"}), false)
            .unwrap();
    seed_old_google_route(&mut accounts[0]);
    let old_generation = accounts[0].probe_generation;
    proxy.state.write().await.pool.insert(accounts);
    proxy.hydrate_google_metadata().await;
    let state = proxy.state.read().await;
    let account = &state.pool.accounts["google.json"];
    assert_eq!(account.project.as_deref(), Some("discovered"));
    assert_google_route_cleared(account);
    assert!(account.probe_generation > old_generation);
}

#[test]
fn provenance_enriches_nonlocal_google_without_relabeling() {
    let mut pool = Pool::default();
    let mut uploaded = pool::parse_credentials(
        "upload.json",
        &json!({"type":"google","access_token":"uploaded-token","refresh_token":"shared-refresh"}),
        false,
    )
    .unwrap()
    .remove(0);
    uploaded.google_client = None;
    uploaded.google_secret = None;
    pool.insert(vec![uploaded]);
    pool::add_provenance_google(
        &mut pool,
        "provenance-label",
        json!({"refresh_token":"shared-refresh","client_id":"client","client_secret":"secret","project_id":"project","email":"person@example.test"}),
    );
    assert_eq!(pool.accounts.len(), 1);
    let account = &pool.accounts["upload.json"];
    assert!(!account.local);
    assert_eq!(account.label, "upload.json");
    assert_eq!(account.google_client.as_deref(), Some("client"));
    assert_eq!(account.google_secret.as_deref(), Some("secret"));
    assert_eq!(account.project.as_deref(), Some("project"));
    assert_eq!(account.email.as_deref(), Some("person@example.test"));
}

#[tokio::test]
async fn local_claude_unauthorized_reloads_and_retries_management_and_quota() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mock = serve(Router::new().route(
        "/api/oauth/usage",
        get({
            let seen = seen.clone();
            move |headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    let token = router::bearer(&headers).unwrap();
                    seen.lock().await.push(token.to_owned());
                    match token {
                        "local-management" if !headers.contains_key("anthropic-beta") => token.to_owned().into_response(),
                        "local-old" | "local-management" => StatusCode::UNAUTHORIZED.into_response(),
                        "local-usage" => axum::Json(json!({"five_hour":{"utilization":25}})).into_response(),
                        _ => panic!("unexpected Claude access token"),
                    }
                }
            }
        }),
    ))
    .await;
    let dir = Scratch::new();
    let profiles = dir.join(".config/meridian-opencode/profiles.json");
    let write = |token| {
        config::write_private(
            &profiles,
            json!({"profiles":{"test":{"access_token":token,"expires_at":Utc::now() + TimeDelta::hours(1)}}}).to_string().as_bytes(),
            true,
        )
        .unwrap();
    };
    write("local-old");
    let mut config = fixture_config(&dir, &mock);
    config.discover_local = true;
    let mut proxy = Proxy::new(config).unwrap();
    proxy.management_mock = Some(mock.0.clone());
    let proxy = Arc::new(proxy);
    let account = proxy.state.read().await.pool.accounts.values().find(|a| a.access == "local-old").unwrap().clone();
    assert!(account.local);
    write("local-management");
    let server = serve(proxy.clone().app()).await;
    let http = Http::new(&server);
    let call = || {
        http.management(
            "POST",
            "/v0/management/api-call",
            Some(json!({
                "auth_index":account.auth_index,"method":"GET","url":mock.0.join("/api/oauth/usage").unwrap().as_str(),
                "header":{"authorization":"Bearer $TOKEN$"}
            })),
        )
    };
    let reply: Value = call().await.json().await.unwrap();
    assert_eq!(reply["status_code"], 200);
    assert_eq!(reply["body"], "[REDACTED]");
    assert_eq!(seen.lock().await.as_slice(), ["local-old", "local-management"]);

    write("local-usage");
    let snapshot = proxy.state.read().await.pool.accounts[&account.id].clone();
    proxy.refresh_usage(&snapshot).await.unwrap();
    assert_eq!(seen.lock().await.as_slice(), ["local-old", "local-management", "local-management", "local-usage"]);
    let state = proxy.state.read().await;
    assert_eq!(state.pool.accounts[&account.id].windows[0].used_percent, Some(25.0));
    assert!(!state.pool.accounts[&account.id].probe_failed);
}

#[test]
fn local_discovery_skips_expired_oauth_and_excluded_providers() {
    const CHILD: &str = "PROXY_DISCOVERY_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = Scratch::new();
        // Isolate discovery's environment inputs from other tests and the host.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::local_discovery_skips_expired_oauth_and_excluded_providers"])
            .env_clear()
            .env(CHILD, "1")
            .env("USER", "fleetix-proxy-discovery-fixture")
            .env("CLAUDE_CONFIG_DIR", dir.join(".claude"))
            .env("CODEX_HOME", dir.join(".codex"))
            .env("XDG_DATA_HOME", dir.join(".local/share"))
            .env("ANTHROPIC_API_KEY", "env-claude")
            .env("OPENAI_API_KEY", "env-codex")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let home = PathBuf::from(std::env::var_os("CODEX_HOME").unwrap()).parent().unwrap().to_owned();
    let endpoint = config::origin(config::DEFAULT_URL).unwrap();
    let mut config = Config {
        bind: config::DEFAULT_BIND.parse().unwrap(),
        home: home.clone(),
        t3code_home: home.clone(),
        auth_dir: home.join("auth"),
        discover_local: true,
        exclude_local_lanes: Default::default(),
        provenance_oauth_dir: home.join("enrollments"),
        provenance_oauth_bin: None,
        client_key: String::new(),
        management_key: String::new(),
        anthropic: endpoint.clone(),
        openai: endpoint.clone(),
        codex: endpoint.clone(),
        antigravity: endpoint,
        url: config::DEFAULT_URL.into(),
    };
    let expired = Utc::now() - TimeDelta::minutes(5);
    let write = |path: &str, body: Value| config::write_private(&home.join(path), body.to_string().as_bytes(), true).unwrap();
    write(".claude/.credentials.json", json!({"access_token":"expired-claude","expires_at":expired}));
    write(".codex/auth.json", json!({"tokens":{"access_token":"expired-codex"},"expires_at":expired}));
    write(
        ".local/share/opencode/auth.json",
        json!({
            "anthropic":{"access":"opencode-claude"},"opencode-go":{"key":"keep-opencode-go"}
        }),
    );
    write("userdata/providers/antigravity/test/antigravity-acp/acp_token.json", json!({"access_token":"t3-google"}));
    write("auth/explicit.json", json!({"type":"codex","access_token":"explicit-codex","expires_at":expired}));
    let base = Pool::load(&config).unwrap();
    let has = |pool: &Pool, token: &str| pool.accounts.values().any(|a| a.access == token);
    for token in ["env-claude", "env-codex", "opencode-claude", "keep-opencode-go", "t3-google", "explicit-codex"] {
        assert!(has(&base, token), "{token} should be loaded");
    }
    for token in ["expired-claude", "expired-codex"] {
        assert!(!has(&base, token), "{token} should be skipped");
    }
    let opencode_claude = base.accounts.values().find(|a| a.access == "opencode-claude").unwrap();
    assert!(Pool::reload_local(&config, opencode_claude).is_some());
    config.exclude_local_lanes = [Lane::Claude, Lane::Codex, Lane::Google].into();
    assert!(Pool::reload_local(&config, opencode_claude).is_none());
    let filtered = Pool::load(&config).unwrap();
    assert_eq!(filtered.accounts.len(), 2);
    assert!(has(&filtered, "keep-opencode-go"));
    assert_eq!(filtered.accounts["explicit.json"].access, "explicit-codex");
    assert!(!filtered.accounts["explicit.json"].local);
}

#[test]
fn google_project_rewrite_preserves_nested_request_bytes_and_key_order() {
    let original = br#"{"model":"gemini-4-neon-high","pro\u006aect" : "old-proj","request":{"z_key":1,"a_key":2,"nested":{"project":"keep-nested"}},"cloudaicompanionProject":"old-proj"}"#;
    let fields = router::ProjectFields::scan(original).unwrap();
    assert!(router::ProjectFields::scan(br#"{"project":"first","pro\u006aect":"duplicate"}"#).is_none());
    let mut acct = pool::parse_credentials("ag-a.json", &json!({"type":"google","access_token":"tok","project_id":"new-proj"}), false)
        .unwrap()
        .remove(0);
    let rewritten = router::request_body(&Bytes::from_static(original), &fields, &acct);
    let expected = br#"{"model":"gemini-4-neon-high","pro\u006aect" : "new-proj","request":{"z_key":1,"a_key":2,"nested":{"project":"keep-nested"}},"cloudaicompanionProject":"new-proj"}"#;
    assert_eq!(rewritten.as_ref(), expected);
    // When project already matches, body is returned untouched.
    acct.project = Some("old-proj".into());
    let unchanged = router::request_body(&Bytes::from_static(original), &fields, &acct);
    assert_eq!(unchanged.as_ref(), original);
}

#[tokio::test]
async fn t3code_algorithmic_verification_suite_passes() {
    let report = crate::verify::run_all().await;
    assert!(report.passed, "verify-t3code suite failed:\n{}", report.format_human());
    assert_eq!(report.total_count, 9);
}
