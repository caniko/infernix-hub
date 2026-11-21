use contracts::providers::{ActivityIngestBatch, ActivityScope, ProviderCapability, ProviderSummary, RequestPage};
use fleetix_hubd::bootstrap::{self, Settings};
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn fixture_providers_and_activity_over_http() {
    let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let admin = domain::sqlx::PgPool::connect(&base).await.unwrap();
    let name = format!("t_{}", Uuid::new_v4().simple());
    domain::sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let mut url = reqwest::Url::parse(&base).unwrap();
    url.set_path(&format!("/{name}"));
    let assets = std::env::temp_dir().join(&name);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut settings = Settings::fixture(url.as_str(), &addr.to_string());
    settings.asset_dir = assets.clone();
    settings.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
    settings.seed_file = Some(settings.registry_dir.join("hub.fixture.json"));
    let (state, _) = bootstrap::build(&settings).await.unwrap();
    for table in ["model_quota_samples", "activity_exchanges"] {
        let sql = format!("select count(*) from {table}");
        let before: i64 = domain::sqlx::query_scalar(&sql).fetch_one(&state.hub.db).await.unwrap();
        assert!(before > 0, "{table} must be seeded");
        let (restarted, _) = bootstrap::build(&settings).await.unwrap();
        let after: i64 = domain::sqlx::query_scalar(&sql).fetch_one(&restarted.hub.db).await.unwrap();
        assert_eq!(before, after, "{table} was reseeded");
        restarted.hub.db.close().await;
    }
    let token = Uuid::new_v4().to_string();
    domain::system::issue_token(&state.hub.db, "operator", contracts::Role::Operator, Some(token.clone())).await.unwrap();
    let db = state.hub.db.clone();
    let server = tokio::spawn(async move { axum::serve(listener, fleetix_hubd::router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    let get = |path: &str| http.get(format!("http://{addr}/v1/{path}")).bearer_auth(&token);
    let post = |path: &str| http.post(format!("http://{addr}/v1/{path}")).bearer_auth(&token);

    let list: Vec<ProviderSummary> = get("providers").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(list.len(), 4);
    let claude = list.iter().find(|p| p.descriptor.id == "claude").unwrap();
    assert!(claude.descriptor.capabilities.contains(&ProviderCapability::SelectLocalAccount));
    let readonly = list.iter().find(|p| p.descriptor.id == "acme-readonly").unwrap();
    assert!(readonly.descriptor.capabilities.is_empty());

    let operation = |account: &str| json!({"idempotency_key":"test-select", "command":{"facet":"account", "payload":{"op":"select_local_account", "account_id":account}}});
    let second = "claude:subscription-2";
    let response = post("providers/acme-readonly/operations").json(&operation(second)).send().await.unwrap();
    assert_eq!(response.status(), 422);
    assert_eq!(response.json::<Value>().await.unwrap()["code"], "capability_unsupported");
    let select = || post("providers/claude/operations").json(&operation(second));
    let first: Value = select().send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let replay: Value = select().send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(first["status"], "succeeded");
    assert_eq!(first["id"], replay["id"]);
    let response = post("providers/claude/operations").json(&operation("claude:subscription-3")).send().await.unwrap();
    assert_eq!(response.status(), 409);
    assert_eq!(response.json::<Value>().await.unwrap()["code"], "idempotency_conflict");
    let changed: ProviderSummary = get("providers/claude").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(changed.active_account_id.as_deref(), Some(second));

    for path in ["overview", "compare", "requests", "traces", "drift", "totals", "thinking", "evals"] {
        let response = get(&format!("activity/{path}")).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}: {response:?}");
        let body: Value = response.json().await.unwrap();
        assert!(body.is_object() || body.is_array(), "{path}: {body}");
    }

    let fixture = integrations::providers::FullFixtureModelProvider::new("claude", "Claude", "claude");
    let mut batch = fixture.seed_activity();
    let mut event = batch.exchanges.pop().unwrap();
    let secret = "sk-ant-test-super-secret-fixture";
    event.source_exchange_id = name.clone();
    event.session_id = Some(name.clone());
    event.occurred_at = chrono::Utc::now();
    event.error_message = Some(format!("Authorization: Bearer {secret}"));
    let batch = ActivityIngestBatch { exchanges: vec![event], ..batch };
    post("activity/ingest").json(&batch).send().await.unwrap().error_for_status().unwrap();
    let page: RequestPage = get("activity/requests")
        .query(&ActivityScope { limit: Some(1), ..Default::default() })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let exchange = page.items.first().unwrap();
    assert_eq!(exchange.session_id.as_deref(), Some(name.as_str()));
    // Exercise the export redactor independently of the ingest redactor.
    domain::sqlx::query(
        "insert into activity_captures (id, exchange_id, representation, payload, size_bytes) values ($1, $2, 'transformed', $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(exchange.id)
    .bind(json!({"authorization":secret,"model":"fixture-model"}))
    .bind(100_i64)
    .execute(&db)
    .await
    .unwrap();
    let preview: Value = post("activity/exports/diagnostic-preview")
        .json(&json!({"exchange_ids":[exchange.id],"include_error_messages":true,"include_transformed_bodies":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preview["exchange_count"], 1);
    assert_eq!(preview["snapshot"]["schema_version"], 1);
    assert!(!preview.to_string().contains(secret));
    assert!(preview.to_string().contains("fixture-model"));
    let archive: Value = post("activity/exports/portable-archive")
        .json(&json!({"session_id":name}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(archive["manifest"]["exchange_count"], 1);
    assert_eq!(archive["manifest"]["capture_count"], 1);
    let jsonl = archive["jsonl"].as_str().unwrap();
    assert!(!jsonl.contains(secret));
    assert!(jsonl.contains("fixture-model"));
    let mut lines = jsonl.lines().map(|line| serde_json::from_str::<Value>(line).unwrap());
    assert_eq!(lines.next().unwrap()["data"], archive["manifest"]);
    assert!(lines.any(|line| line["type"] == "exchange"));

    server.abort();
    let _ = server.await;
    db.close().await;
    domain::sqlx::query(&format!("drop database {name}")).execute(&admin).await.unwrap();
    let _ = std::fs::remove_dir_all(assets);
}
