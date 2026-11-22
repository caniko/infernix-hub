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

    // Verify "success" outcome (from agent proxy) counts as non-failure and keyset cursor paginates ties.
    let tie_ts = chrono::Utc::now() - chrono::Duration::seconds(5);
    let mut tie_events = Vec::new();
    for idx in 0..3 {
        let mut ev = fixture.seed_activity().exchanges.pop().unwrap();
        ev.source_exchange_id = format!("{name}-tie-{idx}");
        ev.session_id = Some(format!("{name}-tie-session"));
        ev.occurred_at = tie_ts;
        ev.outcome = "success".into();
        ev.error_message = None;
        tie_events.push(ev);
    }
    post("activity/ingest")
        .json(&ActivityIngestBatch { source_host: "test-host".into(), source_user: "test-user".into(), exchanges: tie_events })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let p1: RequestPage = get("activity/requests")
        .query(&ActivityScope { search: Some(format!("{name}-tie-session")), limit: Some(2), ..Default::default() })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p1.items.len(), 2);
    let cursor = p1.next_cursor.clone().expect("first page must emit next_cursor");
    let p2: RequestPage = get("activity/requests")
        .query(&ActivityScope { search: Some(format!("{name}-tie-session")), limit: Some(2), cursor: Some(cursor), ..Default::default() })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(p2.items.len(), 1);
    assert!(p2.next_cursor.is_none());
    assert!(!p1.items.iter().any(|r| r.id == p2.items[0].id));

    let failed_ties: RequestPage = get("activity/requests")
        .query(&ActivityScope { search: Some(format!("{name}-tie-session")), outcome: Some("failed".into()), ..Default::default() })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(failed_ties.items.is_empty(), "'success' outcome must not match outcome=failed");

    let tie_overview: Value = get("activity/overview")
        .query(&ActivityScope { search: Some(format!("{name}-tie-session")), ..Default::default() })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tie_overview["requests"], 3);
    assert_eq!(tie_overview["failures"], 0);

    // Verify totals() grand_total is uncapped even when hourly buckets exceed LIMIT 366.
    let base_hour = chrono::Utc::now() - chrono::Duration::days(30);
    for h in 0..370i64 {
        let bucket = base_hour + chrono::Duration::hours(h);
        domain::sqlx::query(
            "insert into activity_totals
                (hour_bucket, source, provider_id, account_id, client, effective_model, effort,
                 requests, failures, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                 thinking_tokens, api_equivalent_cost_usd, unpriced_requests)
             values ($1, 'proxy', 'claude', 'acct-totals', 't3code', 'totals-cap-model', '',
                     1, 0, 10, 5, 0, 0, 0, 0.01, 0)
             on conflict do nothing",
        )
        .bind(bucket)
        .execute(&db)
        .await
        .unwrap();
    }
    let totals_resp: Value = get("activity/totals")
        .query(&[("granularity", "hour"), ("model", "totals-cap-model")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(totals_resp["buckets"].as_array().unwrap().len(), 366);
    assert_eq!(totals_resp["grand_total"]["requests"], 370);

    // Verify collector claim scope grants, active 5-min lease, and receipt ownership.
    let collector_a = Uuid::new_v4().to_string();
    let collector_b = Uuid::new_v4().to_string();
    domain::system::issue_token(&db, "collector-a", contracts::Role::Collector, Some(collector_a.clone())).await.unwrap();
    domain::system::issue_token(&db, "collector-b", contracts::Role::Collector, Some(collector_b.clone())).await.unwrap();
    domain::sqlx::query(
        "insert into model_source_grants (id, principal_name, host, unix_user, provider_ids, can_claim_operations)
         values ($1, 'collector-a', 'host-a', 'alice', '[\"claude\"]'::jsonb, true),
                ($2, 'collector-b', 'host-b', 'bob', '[\"claude\"]'::jsonb, true)",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&db)
    .await
    .unwrap();
    let op_id = Uuid::new_v4();
    domain::sqlx::query(
        "insert into model_operations
            (id, provider_id, actor, idempotency_key, request_sha256, command_kind, command, status, target_host, target_user)
         values ($1, 'claude', 'operator', 'lease-test-op', 'sha-test', 'quota',
                 '{\"facet\":\"quota\",\"payload\":{\"op\":\"refresh_quota\",\"account_id\":null}}'::jsonb, 'pending', 'host-a', 'alice')",
    )
    .bind(op_id)
    .execute(&db)
    .await
    .unwrap();
    // collector-b cannot claim host-a/alice operations.
    let forbidden_claim = http
        .post(format!("http://{addr}/v1/providers/operations/claim"))
        .bearer_auth(&collector_b)
        .json(&json!({"host":"host-a","unix_user":"alice","provider_ids":["claude"],"limit":5}))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden_claim.status(), 403);
    // collector-a claims the pending operation once; immediate re-claim returns empty while leased.
    let claimed_1: Vec<Value> = http
        .post(format!("http://{addr}/v1/providers/operations/claim"))
        .bearer_auth(&collector_a)
        .json(&json!({"host":"host-a","unix_user":"alice","provider_ids":["claude"],"limit":5}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(claimed_1.len(), 1);
    assert_eq!(claimed_1[0]["id"], op_id.to_string());
    let claimed_2: Vec<Value> = http
        .post(format!("http://{addr}/v1/providers/operations/claim"))
        .bearer_auth(&collector_a)
        .json(&json!({"host":"host-a","unix_user":"alice","provider_ids":["claude"],"limit":5}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(claimed_2.is_empty(), "actively leased operation must not be re-claimed");
    // collector-b cannot report a receipt for collector-a's claimed operation.
    let bad_receipt = http
        .post(format!("http://{addr}/v1/providers/operations/{op_id}/receipt"))
        .bearer_auth(&collector_b)
        .json(&json!({"status":"succeeded","receipt":{"ok":true}}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_receipt.status(), 403);

    // collector-b cannot ingest activity for host-a/alice or for an ungranted provider.
    let mut foreign_ev = fixture.seed_activity().exchanges.pop().unwrap();
    foreign_ev.source_exchange_id = format!("{name}-forged-1");
    foreign_ev.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(3);
    let bad_ingest_source = http
        .post(format!("http://{addr}/v1/activity/ingest"))
        .bearer_auth(&collector_b)
        .json(&ActivityIngestBatch { source_host: "host-a".into(), source_user: "alice".into(), exchanges: vec![foreign_ev.clone()] })
        .send()
        .await
        .unwrap();
    assert_eq!(bad_ingest_source.status(), 403);
    foreign_ev.source_exchange_id = format!("{name}-forged-2");
    foreign_ev.provider_id = "opencode".into();
    let bad_ingest_provider = http
        .post(format!("http://{addr}/v1/activity/ingest"))
        .bearer_auth(&collector_b)
        .json(&ActivityIngestBatch { source_host: "host-b".into(), source_user: "bob".into(), exchanges: vec![foreign_ev] })
        .send()
        .await
        .unwrap();
    assert_eq!(bad_ingest_provider.status(), 403);

    // Verify trace_detail computes known_subtotal_cost_usd over reachable descendants.
    let root_sid = format!("{name}-root");
    let child_sid = format!("{name}-child");
    let mut root_ev = fixture.seed_activity().exchanges.pop().unwrap();
    root_ev.source_exchange_id = format!("{name}-trace-root");
    root_ev.session_id = Some(root_sid.clone());
    root_ev.parent_session_id = None;
    root_ev.effective_model = "claude-sonnet-4-6".into();
    root_ev.input_tokens = 1_000_000;
    root_ev.output_tokens = 0;
    root_ev.cache_read_tokens = 0;
    root_ev.cache_write_tokens = 0;
    root_ev.thinking_tokens = 0;
    root_ev.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(4);
    let mut child_ev = root_ev.clone();
    child_ev.source_exchange_id = format!("{name}-trace-child");
    child_ev.session_id = Some(child_sid.clone());
    child_ev.parent_session_id = Some(root_sid.clone());
    child_ev.input_tokens = 2_000_000;
    child_ev.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(3);
    post("activity/ingest")
        .json(&ActivityIngestBatch { source_host: "test-host".into(), source_user: "test-user".into(), exchanges: vec![root_ev, child_ev] })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let trace: Value = get(&format!("activity/traces/{root_sid}")).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let nodes = trace["nodes"].as_array().unwrap();
    let root_node = nodes.iter().find(|n| n["session_id"] == root_sid).unwrap();
    let child_node = nodes.iter().find(|n| n["session_id"] == child_sid).unwrap();
    let root_own = root_node["own_cost_usd"].as_f64().unwrap();
    let child_own = child_node["own_cost_usd"].as_f64().unwrap();
    let root_sub = root_node["known_subtotal_cost_usd"].as_f64().unwrap();
    assert!(root_own > 0.0 && child_own > 0.0);
    assert!((root_sub - (root_own + child_own)).abs() < 1e-6, "root subtotal {root_sub} must equal own {root_own} + child {child_own}");

    // Verify DeleteGroup side effect removes the key group from the database.
    let groups: Vec<Value> = get("providers/claude/key-groups").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let group_id = groups[0]["id"].as_str().expect("fixture provider must have a seeded key group").to_string();
    let del_op: Value = post("providers/claude/operations")
        .json(&json!({
            "idempotency_key": "delete-temp-group",
            "command": {"facet": "key_group", "payload": {"op": "delete_group", "group_id": group_id}}
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del_op["status"], "succeeded");
    let group_still_exists: bool = domain::sqlx::query_scalar("select exists(select 1 from model_key_groups where id = $1)")
        .bind(&group_id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(!group_still_exists, "delete_group operation must remove model_key_groups row");

    // Verify RunCanary persists into model_probe_runs and surfaces in activity/drift.
    let canary_op: Value = post("providers/claude/operations")
        .json(&json!({
            "idempotency_key": "canary-persist-test",
            "command": {"facet": "probe", "payload": {"op": "run_canary", "model_id": "claude-sonnet-4-6", "effort": "medium", "account_id": null}}
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(canary_op["status"], "succeeded");
    let drift: Value = get("activity/drift").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert!(
        drift["recent_canaries"].as_array().unwrap().iter().any(|c| c["id"] == canary_op["id"]),
        "completed canary must appear in drift.recent_canaries"
    );

    // Verify duplicate receipt retries are idempotent and do not re-apply routing revision bumps.
    let route_op_id = Uuid::new_v4();
    domain::sqlx::query(
        "insert into model_operations
            (id, provider_id, actor, idempotency_key, request_sha256, command_kind, command, status, target_host, target_user)
         values ($1, 'claude', 'operator', 'idemp-receipt-op', 'sha-idemp', 'routing',
                 '{\"facet\":\"routing\",\"payload\":{\"op\":\"set_pool_override\",\"account_id\":null}}'::jsonb,
                 'pending', 'host-a', 'alice')",
    )
    .bind(route_op_id)
    .execute(&db)
    .await
    .unwrap();
    let claimed_route: Vec<Value> = http
        .post(format!("http://{addr}/v1/providers/operations/claim"))
        .bearer_auth(&collector_a)
        .json(&json!({"host":"host-a","unix_user":"alice","provider_ids":["claude"],"limit":5}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(claimed_route.len(), 1);
    assert_eq!(claimed_route[0]["id"], route_op_id.to_string());
    let rev_before: i64 = domain::sqlx::query_scalar("select revision from provider_routing_state where provider_id = 'claude'")
        .fetch_one(&db)
        .await
        .unwrap();
    for _ in 0..2 {
        http.post(format!("http://{addr}/v1/providers/operations/{route_op_id}/receipt"))
            .bearer_auth(&collector_a)
            .json(&json!({"status":"succeeded","receipt":{"applied":true}}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let rev_after: i64 = domain::sqlx::query_scalar("select revision from provider_routing_state where provider_id = 'claude'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(rev_after, rev_before + 1, "duplicate receipt retry must not re-apply side effects");

    // Verify export_evals?include_bodies=false omits replay response_preview as well as prompt_text.
    let eval_created: Value = post("activity/evals")
        .json(&json!({
            "name": "body-redaction-eval",
            "provider_id": "claude",
            "model_id": "claude-sonnet-4-6",
            "prompt_text": "SECRET_PROMPT_BODY",
            "expected_outcome": "ok",
            "predicates": []
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let eval_id = Uuid::parse_str(eval_created["id"].as_str().unwrap()).unwrap();
    domain::sqlx::query(
        "insert into model_eval_replays (id, case_id, model_id, actual_outcome, response_preview, passed)
         values ($1, $2, 'claude-sonnet-4-6', 'ok', 'SECRET_REPLAY_RESPONSE', true)",
    )
    .bind(Uuid::new_v4())
    .bind(eval_id)
    .execute(&db)
    .await
    .unwrap();
    let exported: Value = get("activity/evals/export")
        .query(&[("include_bodies", "false")])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!exported.to_string().contains("SECRET_PROMPT_BODY"));
    assert!(!exported.to_string().contains("SECRET_REPLAY_RESPONSE"));

    server.abort();
    let _ = server.await;
    db.close().await;
    domain::sqlx::query(&format!("drop database {name} with (force)")).execute(&admin).await.unwrap();
    let _ = std::fs::remove_dir_all(assets);
}
