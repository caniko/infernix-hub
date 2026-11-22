use contracts::providers::{ActivityIngestBatch, ActivityScope, ProviderCapability, ProviderSummary, RequestPage};
use fleetix_hubd::bootstrap::{self, Settings};
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
async fn provider_account_errors_survive_migration_and_resolve_independently_over_http() {
    use chrono::{Duration, Utc};
    use contracts::providers::*;
    use contracts::{NotificationPage, Severity};
    use domain::{MIGRATOR, ports::ModelProvider, sqlx};

    let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    let name = format!("t_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let mut url = reqwest::Url::parse(&base).unwrap();
    url.set_path(&format!("/{name}"));
    let db = sqlx::PgPool::connect(url.as_str()).await.unwrap();
    let before_17 = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Borrowed(&MIGRATOR.migrations[..MIGRATOR.migrations.partition_point(|m| m.version < 17)]),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before_17.run(&db).await.unwrap();
    sqlx::query(
        "insert into model_provider_instances (id, label, family, mode, verification, health)
         values ('test-proxy', 'Test Proxy', 'test', 'fixture', 'fixture_tested', 'degraded')",
    )
    .execute(&db)
    .await
    .unwrap();
    sqlx::query("insert into model_accounts (provider_id, id, source_account_id, display_name) values ('test-proxy', 'primary', 'primary', 'Primary')")
        .execute(&db)
        .await
        .unwrap();
    MIGRATOR.run(&db).await.unwrap();
    let fields: (Option<String>, i32, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("select error_kind, error_consecutive_count, error_backoff_until from model_accounts").fetch_one(&db).await.unwrap();
    assert_eq!(fields, (None, 0, None));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut settings = Settings::fixture(url.as_str(), &addr.to_string());
    settings.asset_dir = std::env::temp_dir().join(&name);
    settings.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
    settings.seed_file = Some(settings.registry_dir.join("hub.fixture.json"));
    let (state, _) = bootstrap::build(&settings).await.unwrap();
    let token = Uuid::new_v4().to_string();
    domain::system::issue_token(&state.hub.db, "operator", contracts::Role::Operator, Some(token.clone())).await.unwrap();
    let hub = state.hub.clone();
    let server = tokio::spawn(async move { axum::serve(listener, fleetix_hubd::router(state)).await.unwrap() });
    let http = reqwest::Client::new();
    let get = |path: &str| http.get(format!("http://{addr}/v1/{path}")).bearer_auth(&token);
    let post_batch = |batch: &ObservationBatch| {
        http.post(format!("http://{addr}/v1/providers/test-proxy/observations")).bearer_auth(&token).json(batch).send()
    };
    let set_mask = |mask_emails| {
        http.put(format!("http://{addr}/v1/providers/settings"))
            .bearer_auth(&token)
            .json(&ProviderPanelSettings { mask_emails, ..Default::default() })
            .send()
    };
    let alerts = || async {
        get("notifications?state=all").send().await.unwrap().error_for_status().unwrap().json::<NotificationPage>().await.unwrap().items
    };
    let get_accounts = || async {
        get("providers/test-proxy/accounts").send().await.unwrap().error_for_status().unwrap().json::<Vec<ModelAccount>>().await.unwrap()
    };
    let mut batch =
        integrations::providers::FullFixtureModelProvider::new("test-proxy", "Test Proxy", "test").collect().await.unwrap().unwrap();
    batch.accounts.truncate(2);
    batch.accounts[0].id = "primary".into(); // exercises the pre-0017 row's conflict update
    let email = "verification-person@example.test";
    batch.accounts[0].email = Some(email.into());
    batch.accounts[0].masked_email = None;
    batch.accounts[0].enabled = true;
    batch.accounts[0].is_next_pick = true;
    batch.accounts[0].cooldown_until = None;
    batch.accounts[0].enrollment_state = EnrollmentState::Verified;
    batch.accounts[1].id = "backup".into();
    batch.accounts[1].enabled = true;
    let at = chrono::DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
    batch.collected_at = at;
    batch.accounts[1].cooldown_until = Some(at + Duration::minutes(5));
    batch.reset_grants.clear();
    batch.health = ProviderHealth::Degraded;
    for account in &mut batch.accounts {
        account.last_error_observed = true;
    }
    batch.account_inventory_complete = true;
    batch.source_publisher = Some("fleetix-agent-proxy".into());
    let error = AccountError {
        kind: AccountErrorKind::VerificationRequired,
        source: AccountErrorSource::OAuthRefresh,
        http_status: Some(403),
        provider_code: Some("VALIDATION_REQUIRED".into()),
        message: "Verify your account".into(),
        action_hint: Some(format!("Sign in again as {email}")),
        action_url: Some("https://example.com".into()),
        occurred_at: at,
        consecutive_count: 1,
        backoff_until: Some(at + Duration::minutes(15)),
    };
    batch.accounts[0].last_error = Some(error.clone());
    batch.accounts[1].last_error = Some(AccountError { kind: AccountErrorKind::QuotaExhausted, backoff_until: None, ..error.clone() });
    set_mask(false).await.unwrap().error_for_status().unwrap();
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    let persisted: (String, String) =
        sqlx::query_as("select mode, verification from model_provider_instances where id = 'test-proxy'").fetch_one(&db).await.unwrap();
    assert_eq!(persisted, (contracts::AdapterMode::Live.as_str().into(), contracts::Verification::LiveVerified.as_str().into()));
    let newer_fixture_at = at + Duration::seconds(1);
    sqlx::query(
        "insert into model_provider_instances (id, label, family, mode, verification, health, last_observed_at)
         values ('newer-fixture', 'Newer Fixture', 'test', 'fixture', 'fixture_tested', 'healthy', $1)",
    )
    .bind(newer_fixture_at)
    .execute(&db)
    .await
    .unwrap();
    let older_live: ObservationBatch = serde_json::from_value(json!({
        "provider_id":"newer-fixture", "provider_label":"Older Live", "provider_family":"test",
        "source_host":"test", "source_user":"test", "collected_at":at, "health":"offline"
    }))
    .unwrap();
    let operator = domain::providers::internal_operator("test");
    domain::providers::ingest_observations(&hub, &operator, "newer-fixture", older_live).await.unwrap();
    let persisted: (String, String, String, chrono::DateTime<Utc>) =
        sqlx::query_as("select mode, verification, health, last_observed_at from model_provider_instances where id = 'newer-fixture'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(persisted, ("live".into(), "live_verified".into(), "healthy".into(), newer_fixture_at));
    let loaded = get_accounts().await;
    let primary = loaded.iter().find(|a| a.id == "primary").unwrap();
    assert_eq!(primary.last_error.as_ref(), Some(&error));
    assert_eq!(primary.status, AccountStatus::Error);
    assert_eq!(loaded.iter().find(|a| a.id == "backup").unwrap().last_error.as_ref().unwrap().kind, AccountErrorKind::QuotaExhausted);
    let error_key = "provider:test-proxy:primary:error";
    let pool_key = "provider:test-proxy:pool:unavailable";
    let quota_key = "provider:test-proxy:backup:cooldown";
    let items = alerts().await;
    let find = |items: &[contracts::Notification], key: &str| items.iter().find(|n| n.dedupe_key == key).unwrap().clone();
    let first_alert = find(&items, error_key);
    assert_eq!(first_alert.severity, Severity::Critical);
    assert!(first_alert.title.ends_with("requires verification"));
    assert!(first_alert.title.contains(email) && first_alert.body.contains(email));
    assert!(first_alert.body.contains("VALIDATION_REQUIRED"));
    let first_pool_alert = find(&items, pool_key);
    assert!(first_pool_alert.resolved_at.is_none());
    assert!(first_pool_alert.body.contains(email));
    assert!(find(&items, quota_key).resolved_at.is_none());
    assert!(!items.iter().any(|n| n.dedupe_key == "provider:test-proxy:backup:error"));

    let post = |path: &str| http.post(format!("http://{addr}/v1/{path}")).bearer_auth(&token);
    post("notifications/ack").json(&json!({"ids":[first_alert.id,first_pool_alert.id]})).send().await.unwrap().error_for_status().unwrap();
    let items = alerts().await;
    let ack = find(&items, error_key);
    let pool_ack = find(&items, pool_key);
    assert!(pool_ack.read_at.is_some());
    let ack_seq = ack.seq;
    set_mask(true).await.unwrap().error_for_status().unwrap();
    let items = alerts().await;
    let masked = find(&items, error_key);
    let masked_email = mask_email(email);
    assert!(masked.title.contains(&masked_email) && masked.body.contains(&masked_email));
    assert!(!masked.title.contains(email) && !masked.body.contains(email));
    assert_eq!((masked.read_at, masked.seq), (ack.read_at, ack.seq));
    let masked_pool = find(&items, pool_key);
    assert!(masked_pool.body.contains(&masked_email));
    assert!(!masked_pool.title.contains(email) && !masked_pool.body.contains(email));
    assert_eq!(masked_pool.title, pool_ack.title);
    assert_eq!(masked_pool.resolved_at, None);
    assert_eq!((masked_pool.read_at, masked_pool.seq), (pool_ack.read_at, pool_ack.seq));
    batch.accounts[0].last_error.as_mut().unwrap().message = "Please verify. Request ID: unique-2".into();
    batch.accounts[0].last_error.as_mut().unwrap().consecutive_count = 2;
    batch.accounts[0].status_message = None;
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    let acknowledged = find(&alerts().await, error_key);
    assert!(acknowledged.read_at.is_some());
    assert!(!acknowledged.body.contains("unique-2"));
    assert_eq!(acknowledged.seq, ack_seq);
    assert!(get_accounts().await.iter().find(|a| a.id == "primary").unwrap().status_message.as_ref().unwrap().contains("unique-2"));
    batch.accounts[0].last_error.as_mut().unwrap().provider_code = Some("NEW_DIAGNOSTIC_CODE".into());
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    let updated = find(&alerts().await, error_key);
    assert!(updated.body.contains("NEW_DIAGNOSTIC_CODE"));
    assert_eq!(updated.seq, ack_seq);

    let mut legacy = batch.clone();
    legacy.accounts[0].last_error_observed = false;
    legacy.account_inventory_complete = false;
    legacy.accounts[0].last_error = None;
    legacy.accounts[0].status_message = Some("legacy diagnostic".into());
    legacy.accounts.truncate(1);
    post_batch(&legacy).await.unwrap().error_for_status().unwrap();
    assert_eq!(get_accounts().await.len(), 2, "omission from a partial inventory is not removal");
    assert_eq!(get_accounts().await.iter().find(|a| a.id == "primary").unwrap().last_error.as_ref().unwrap().consecutive_count, 2);

    batch.health = ProviderHealth::Degraded;
    batch.accounts[0].last_error = Some(AccountError {
        kind: AccountErrorKind::PermissionDenied,
        provider_code: None,
        action_hint: None,
        action_url: None,
        consecutive_count: 2,
        ..error.clone()
    });
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    let items = alerts().await;
    let updated = find(&items, error_key);
    assert_eq!(updated.id, first_alert.id);
    assert_eq!(updated.severity, Severity::Warning);
    assert!(updated.title.ends_with("permission denied"));
    assert!(!updated.body.contains("VALIDATION_REQUIRED"));
    assert!(updated.read_at.is_none());
    assert!(updated.seq > ack_seq);

    batch.health = ProviderHealth::Healthy;
    batch.accounts[0].last_error = None;
    let mut delayed = batch.clone();
    delayed.collected_at -= Duration::minutes(1);
    post_batch(&delayed).await.unwrap().error_for_status().unwrap();
    assert!(get_accounts().await.iter().find(|a| a.id == "primary").unwrap().last_error.is_some());
    let previous_observed = batch.accounts[0].last_observed_at.unwrap_or(batch.collected_at);
    batch.accounts[0].last_observed_at = Some(previous_observed - Duration::minutes(1));
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    assert!(get_accounts().await.iter().find(|a| a.id == "primary").unwrap().last_error.is_some());
    batch.accounts[0].last_observed_at = Some(previous_observed);
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    let loaded = get_accounts().await;
    let primary = loaded.iter().find(|a| a.id == "primary").unwrap();
    assert!(primary.last_error.is_none());
    assert_eq!(primary.status, AccountStatus::NextPick);
    let cleared: bool = sqlx::query_scalar(
        "select num_nonnulls(error_kind, error_source, error_http_status, error_code, error_message, error_action_hint, error_action_url, error_at, error_backoff_until) = 0
            and error_consecutive_count = 0 from model_accounts where provider_id = 'test-proxy' and id = 'primary'",
    ).fetch_one(&db).await.unwrap();
    assert!(cleared);
    let items = alerts().await;
    assert!(find(&items, error_key).resolved_at.is_some());
    assert!(find(&items, pool_key).resolved_at.is_some());
    assert!(find(&items, quota_key).resolved_at.is_none());

    // First establish a second push publisher; the fresh one must not hide it.
    let mut other = batch.clone();
    other.source_host = "other-live-host".into();
    other.collected_at = at - Duration::minutes(11);
    other.accounts.clear();
    other.status_message = Some("Cookie: session=opaque-header; other=opaque-cookie".into());
    post_batch(&other).await.unwrap().error_for_status().unwrap();
    let stale = alerts().await.into_iter().find(|n| n.title.ends_with("collector stopped reporting")).unwrap();
    assert!(stale.body.contains("other-live-host"));
    let stale_key = stale.dedupe_key.as_str();
    hub.expected_offline_hosts.write().unwrap().push("fixture-host".into());
    domain::providers::reconcile_provider_health_notifications(&hub).await.unwrap();
    assert!(find(&alerts().await, stale_key).resolved_at.is_none());
    hub.expected_offline_hosts.write().unwrap().push("other-live-host".into());
    domain::providers::reconcile_provider_health_notifications(&hub).await.unwrap();
    assert!(find(&alerts().await, stale_key).resolved_at.is_some());
    fleetix_hubd::notify_pass(&hub, Some(std::path::Path::new("/nonexistent-fleet-status-fixture.json")), None, None).await;
    assert!(hub.expected_offline_hosts.read().unwrap().iter().any(|host| host == "other-live-host"));
    hub.expected_offline_hosts.write().unwrap().clear();
    other.collected_at = at;
    post_batch(&other).await.unwrap().error_for_status().unwrap();
    other.collected_at = at - Duration::minutes(12);
    post_batch(&other).await.unwrap().error_for_status().unwrap();
    assert!(find(&alerts().await, stale_key).resolved_at.is_some());
    let freshest: chrono::DateTime<Utc> = sqlx::query_scalar(
        "select last_observed_at from model_provider_publishers where provider_id = 'test-proxy' and source_host = 'other-live-host'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(freshest, at);

    // A complete inventory owns only its publisher's rows and resolves removals.
    other.collected_at = at + Duration::seconds(1);
    let mut foreign_account = batch.accounts[1].clone();
    foreign_account.id = "other-account".into();
    foreign_account.enabled = false;
    other.accounts.push(foreign_account);
    post_batch(&other).await.unwrap().error_for_status().unwrap();
    batch.accounts[0].last_error = Some(error.clone());
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    assert!(find(&alerts().await, error_key).resolved_at.is_none());
    assert!(find(&alerts().await, error_key).read_at.is_none());
    batch.accounts.remove(0);
    batch.status_message = Some("Cookie: a=opaque-header; b=opaque-cookie".into());
    batch.accounts[0].status_message = Some("clientSecret=opaque-legacy".into());
    batch.accounts[0].last_error = None;
    batch.accounts[0].windows[0].error_reason = Some("refreshToken=opaque-quota".into());
    batch.accounts[0].windows[0].observed_at = Some(at + Duration::seconds(1));
    batch.collected_at = at + Duration::seconds(1);
    let mut cli = batch.clone();
    cli.collected_at = at + Duration::seconds(2);
    cli.account_inventory_complete = false;
    cli.accounts[0].id = "cli-only-account".into();
    cli.accounts[0].enabled = false;
    cli.source_publisher = Some("fleetix-cli-collector".into());
    post_batch(&cli).await.unwrap().error_for_status().unwrap();
    let mut legacy_complete = batch.clone();
    legacy_complete.source_publisher = None;
    post_batch(&legacy_complete).await.unwrap().error_for_status().unwrap();
    assert!(get_accounts().await.iter().any(|a| a.id == "primary"), "legacy snapshots cannot remove accounts");
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    let loaded = get_accounts().await;
    assert_eq!(loaded.len(), 3);
    assert!(loaded.iter().any(|a| a.id == "other-account"));
    assert!(loaded.iter().any(|a| a.id == "cli-only-account"));
    assert!(!loaded.iter().any(|a| a.id == "primary"));
    let backup = loaded.iter().find(|a| a.id == "backup").unwrap();
    assert!(find(&alerts().await, error_key).resolved_at.is_some());
    assert!(!backup.status_message.as_ref().unwrap().contains("opaque-legacy"));
    assert!(backup.windows.iter().any(|w| w.error_reason.as_deref() == Some("[redacted]")));
    let saved: Option<String> =
        sqlx::query_scalar("select status_message from model_provider_instances where id = 'test-proxy'").fetch_one(&db).await.unwrap();
    assert_eq!(saved.as_deref(), Some("[redacted]"));

    // One transient diagnostic isn't an outage; a persisted active backoff is.
    batch.accounts[0].cooldown_until = None;
    batch.accounts[0].enrollment_state = EnrollmentState::Verified;
    batch.accounts[0].last_error = Some(AccountError {
        kind: AccountErrorKind::UpstreamUnavailable,
        source: AccountErrorSource::Inference,
        consecutive_count: 1,
        backoff_until: None,
        ..error.clone()
    });
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    assert_ne!(get_accounts().await.iter().find(|a| a.id == "backup").unwrap().status, AccountStatus::Error);
    assert!(find(&alerts().await, pool_key).resolved_at.is_some());
    let diagnostic = batch.accounts[0].last_error.as_mut().unwrap();
    diagnostic.consecutive_count = 3;
    diagnostic.backoff_until = Some(at + Duration::minutes(2));
    post_batch(&batch).await.unwrap().error_for_status().unwrap();
    assert_eq!(get_accounts().await.iter().find(|a| a.id == "backup").unwrap().status, AccountStatus::Error);
    assert!(find(&alerts().await, pool_key).resolved_at.is_none());

    let entry = |source: &str, key: &str| {
        json!({
            "source":source, "dedupe_key":key, "severity":"warning", "title":"Fixture", "body":"Fixture body"
        })
    };
    for (name, role) in [("fixture-agent", contracts::Role::Agent), ("fixture-procedure", contracts::Role::Procedure)] {
        let writer = Uuid::new_v4().to_string();
        domain::system::issue_token(&db, name, role, Some(writer.clone())).await.unwrap();
        let send = |path: &str, body: Value| http.post(format!("http://{addr}/v1/{path}")).bearer_auth(&writer).json(&body).send();
        for prefix in ["providers:", "providers-health:", "providers-poller:"] {
            let source = format!("{prefix}test-proxy");
            assert_eq!(send("notifications", entry(&source, "allowed-key")).await.unwrap().status(), 422);
            assert_eq!(send("notifications/sync", json!({"source":source, "items":[]})).await.unwrap().status(), 422);
            assert_eq!(
                send("notifications/sync", json!({"source":source, "items":[entry(&source, "allowed-key")]})).await.unwrap().status(),
                422
            );
        }
        assert_eq!(send("notifications", entry(name, pool_key)).await.unwrap().status(), 422);
        assert_eq!(send("notifications/sync", json!({"source":name,"items":[entry(name, pool_key)]})).await.unwrap().status(), 422);
    }
    post("notifications").json(&entry("owner-a", "shared-fixture-key")).send().await.unwrap().error_for_status().unwrap();
    assert_eq!(post("notifications").json(&entry("owner-b", "shared-fixture-key")).send().await.unwrap().status(), 422);
    assert_eq!(
        post("notifications/sync")
            .json(&json!({"source":"owner-b", "items":[entry("owner-b","shared-fixture-key")]}))
            .send()
            .await
            .unwrap()
            .status(),
        422
    );
    post("notifications/sync").json(&json!({"source":"owner-b", "items":[]})).send().await.unwrap().error_for_status().unwrap();
    assert!(find(&alerts().await, "shared-fixture-key").resolved_at.is_none());
    let conflict = domain::notifications::Raise {
        source: "owner-b".into(),
        dedupe_key: "shared-fixture-key".into(),
        severity: Severity::Critical,
        title: "Conflicting writer".into(),
        body: String::new(),
        subject_type: None,
        subject_id: None,
        url: None,
    };
    assert!(domain::notifications::raise(&hub, &conflict).await.is_err());
    println!("migration 0017 upgrade and provider health alert round-trip verified against PostgreSQL");

    server.abort();
    let _ = server.await;
    hub.db.close().await;
    db.close().await;
    sqlx::query(&format!("drop database {name} with (force)")).execute(&admin).await.unwrap();
    let _ = std::fs::remove_dir_all(settings.asset_dir);
}

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

    assert_quota_plot_history(&db, get("providers/claude/quota")).await;

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

    let provider_path = "providers/antigravity";
    let seeded: ProviderSummary = get(provider_path).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let accounts: Vec<_> = (0..4)
        .map(|i| {
            let id = format!("antigravity:atlas:tester:live-{i}");
            let mut account = seeded.accounts[0].clone();
            account.id = id;
            account.enabled = true;
            account.credential_fingerprint = None;
            account
        })
        .collect();
    let mut routing: Value = get("providers/antigravity/routing").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    routing["router_id"] = json!("fleetix-proxy");
    routing["runtime"]["router_id"] = json!("fleetix-proxy");
    let mut batch = json!({
        "provider_id": "antigravity", "provider_label": "Antigravity", "provider_family": "antigravity",
        "source_host": "atlas", "source_user": "tester", "collected_at": chrono::Utc::now(),
        "source_publisher": "fleetix-agent-proxy", "account_inventory_complete": true,
        "router_id": "fleetix-proxy", "routing_status": routing, "accounts": accounts,
    });
    let observe = |batch: &Value| post("providers/antigravity/observations").json(batch);
    observe(&batch).send().await.unwrap().error_for_status().unwrap();
    let current: ProviderSummary = get(provider_path).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(current.account_count, 4);
    assert_eq!(current.enabled_accounts, 4);
    assert_eq!(current.pools[0].eligible_accounts, 4);
    assert_eq!(current.accounts.len(), seeded.accounts.len() + 4);
    assert!(current.routed_account_ids.unwrap().iter().all(|id| id.starts_with("antigravity:atlas:tester:")));
    let quota: contracts::providers::ProviderQuotaReport =
        get("providers/antigravity/quota").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(quota.accounts.len(), 4);
    assert!(!quota.samples.is_empty());
    assert!(quota.samples.iter().all(|s| s.account_id.starts_with("antigravity:atlas:tester:")));

    batch["accounts"] = json!([]);
    batch["account_inventory_complete"] = json!(false);
    observe(&batch).send().await.unwrap().error_for_status().unwrap();
    let current: ProviderSummary = get(provider_path).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(current.account_count, 4, "partial reports cannot set membership");

    batch["account_inventory_complete"] = json!(true);
    observe(&batch).send().await.unwrap().error_for_status().unwrap();
    batch["router_id"] = json!("other-router");
    batch["routing_status"]["router_id"] = json!("other-router");
    observe(&batch).send().await.unwrap().error_for_status().unwrap();
    let current: ProviderSummary = get(provider_path).send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(current.account_count, 0);
    assert!(current.routed_account_ids.unwrap().is_empty());
    assert_eq!(current.accounts.len(), seeded.accounts.len() + 4, "an empty pool does not imply empty inventory");
    let quota: contracts::providers::ProviderQuotaReport =
        get("providers/antigravity/quota").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert!(quota.accounts.is_empty() && quota.pools.is_empty() && quota.samples.is_empty());
    let inventory: Vec<contracts::providers::ModelAccount> =
        get("providers/antigravity/accounts").send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(inventory.len(), current.accounts.len(), "inventory remains accessible independently of the pool");
    let historical: contracts::providers::ProviderQuotaReport = get("providers/antigravity/quota")
        .query(&[("account_id", &seeded.accounts[0].id)])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!historical.samples.is_empty());
    assert!(historical.samples.iter().all(|s| s.account_id == seeded.accounts[0].id));

    server.abort();
    let _ = server.await;
    db.close().await;
    domain::sqlx::query(&format!("drop database {name} with (force)")).execute(&admin).await.unwrap();
    let _ = std::fs::remove_dir_all(assets);
}

async fn assert_quota_plot_history(db: &domain::sqlx::PgPool, request: reqwest::RequestBuilder) {
    use chrono::{DateTime, Duration, Utc};

    let latest = DateTime::from_timestamp((Utc::now() - Duration::minutes(1)).timestamp(), 0).unwrap();
    domain::sqlx::query(
        "insert into model_quota_samples
             (provider_id, account_id, window_key, label, scope_model, used_percent, observed_at, observation_state)
         select 'claude', 'claude:subscription-2', 'plot-test', 'Plot test', scope_model, used,
                $1::timestamptz + offset_s * interval '1 second', 'measured'
         from (
             select 'pro' as scope_model, 1.0 as used, -7200 - n as offset_s from generate_series(1, 4001) as n
             union all values ('pro', 80.0, -1800), ('flash', 10.0, -1800), ('pro', 99.0, 0), ('flash', 11.0, 0),
                              ('pro', 100.0, 3600), (null, 10.0, -60)
         ) samples",
    )
    .bind(latest)
    .execute(db)
    .await
    .unwrap();
    domain::sqlx::query(
        "insert into model_quota_samples
             (provider_id, account_id, window_key, label, scope_model, used_percent, observed_at, observation_state)
         values ('claude', 'claude:subscription-2', 'plot-test', 'Plot test', '', 12, $1, 'measured')",
    )
    .bind(latest - Duration::minutes(1))
    .execute(db)
    .await
    .unwrap();
    let corrected_id: i64 = domain::sqlx::query_scalar(
        "insert into model_quota_samples
             (provider_id, account_id, window_key, label, scope_model, used_percent, observed_at, observation_state)
         values ('claude', 'claude:subscription-2', 'plot-test', 'Plot test', 'pro', 82, $1, 'measured')
         returning id",
    )
    .bind(latest)
    .fetch_one(db)
    .await
    .unwrap();
    let report =
        |hours| request.try_clone().unwrap().query(&[("range_hours", hours), ("lookback_hours", "1"), ("window_key", "plot-test")]).send();
    let wide: Value = report("24").await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(wide["history_truncated"], true);
    assert_eq!(wide["samples"].as_array().unwrap().len(), 4000);
    assert_eq!(wide["samples"][3999]["id"], corrected_id);

    let narrow: Value = report("1").await.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(narrow["history_truncated"], false);
    let samples = narrow["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 5);
    assert_eq!(samples[4]["id"], corrected_id);
    assert_eq!(samples[4]["scope_model"], "pro");
    assert_eq!(samples[3]["scope_model"], "flash");
    assert_eq!(samples[2]["scope_model"], Value::Null);
    assert_eq!(samples[2]["used_percent"], 12.0);
    let account = narrow["accounts"].as_array().unwrap().iter().find(|a| a["id"] == "claude:subscription-2").unwrap();
    let pro = account["windows"].as_array().unwrap().iter().find(|w| w["window_key"] == "plot-test" && w["scope_model"] == "pro").unwrap();
    assert_eq!(pro["used_percent"], 82.0, "the report snapshot must also exclude future samples");
    assert_eq!(narrow["past_windows"], json!([]));
    for (model, rate) in [("pro", 4.0), ("flash", 2.0)] {
        for report in [&wide, &narrow] {
            let forecast = report["forecasts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["scope_model"] == model && f["account_id"] == "claude:subscription-2")
                .unwrap();
            assert_eq!(forecast["drain_pct_per_hour"], rate, "a capped history must not starve recent forecasts");
        }
    }
}
