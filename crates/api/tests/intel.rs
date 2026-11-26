use contracts::{
    client::{ClientError, HubClient},
    *,
};
use fleetix_hubd::bootstrap::{self, Settings};
use serde_json::{Value, json};
use uuid::Uuid;

fn api_error<T: std::fmt::Debug>(result: Result<T, ClientError>, status: u16, code: &str) {
    assert!(matches!(result.unwrap_err(), ClientError::Api { status: got, error } if got == status && error.code == code));
}

#[tokio::test]
async fn intel_http() {
    let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let admin = domain::sqlx::PgPool::connect(&base).await.unwrap();
    let name = format!("intel_{}", Uuid::new_v4().simple());
    domain::sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let (head, query) = base.split_once('?').unwrap_or((&base, ""));
    let db_url = format!("{}/{name}?{query}", head.rsplit_once('/').unwrap().0);
    let temp = std::env::temp_dir().join(&name);
    std::fs::create_dir(&temp).unwrap();
    let config = temp.join("intel.json");
    std::fs::write(
        &config,
        br#"{
        "sources":{"acme.feed":{"company_id":"acme","default_audience":"company_internal",
            "kinds":["change"],"trust_tier":"llm","stale_after_secs":3600,"max_signals_per_day":3}},
        "grants":{
            "writer":{"role":"ingest","company_id":"acme","max_audience":"company_internal","write_sources":["acme.feed"]},
            "reader":{"role":"agent","company_id":"other","max_audience":"company_internal"}
        }
    }"#,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut settings = Settings::fixture(&db_url, &listener.local_addr().unwrap().to_string());
    settings.asset_dir = temp.join("assets");
    settings.company_id = Some("studio".into());
    settings.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
    settings.seed_file = Some(settings.registry_dir.join("hub.fixture.json"));
    settings.intel_config = Some(config);
    let (state, _) = bootstrap::build(&settings).await.unwrap();
    for (name, role, token) in [
        ("operator", Role::Operator, "operator-token"),
        ("writer", Role::Ingest, "writer-token"),
        ("reader", Role::Agent, "reader-token"),
        ("ordinary", Role::Agent, "ordinary-token"),
    ] {
        domain::system::issue_token(&state.hub.db, name, role, Some(token.into())).await.unwrap();
    }
    let (project, persona) = (Uuid::new_v4(), Uuid::new_v4());
    domain::sqlx::query("insert into projects (id,company_id,name,kind) values ($1,'acme','Acme','solo')")
        .bind(project)
        .execute(&state.hub.db)
        .await
        .unwrap();
    domain::sqlx::query("insert into personas (id,company_id,name) values ($1,'acme','Acme reviewer')")
        .bind(persona)
        .execute(&state.hub.db)
        .await
        .unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, fleetix_hubd::router(state)).await.unwrap() });
    let op = HubClient::new(&settings.public_url, "operator-token");
    let writer = HubClient::new(&settings.public_url, "writer-token");
    let reader = HubClient::new(&settings.public_url, "reader-token");
    let ordinary = HubClient::new(&settings.public_url, "ordinary-token");
    let sources = op.intel_sources().await.unwrap();
    assert_eq!(sources[0].id, "acme.feed");
    assert!(sources[0].stale);
    assert!(reader.intel_sources().await.unwrap().is_empty());
    api_error(ordinary.intel_sources().await, 403, "no_intel_grant");
    let news: IngestReceipt = writer
        .post("/ingest/news", &json!({
            "source":"n8n:release-feed","external_id":"v1","url":"https://github.com/example/project/releases/tag/v1","title":"Release v1"
        }))
        .await
        .unwrap();
    let mut body = json!({
        "source":"acme.feed",
        "run":{"run_id":"run-1","status":"partial","finished_at":"2026-01-01T00:00:00Z",
            "error_code":"upstream","error_detail":"internal diagnostics","watermark":{"page":1}},
        "signals":[{
            "external_id":"event-1","kind":"change","severity":"critical","title":"Quarterly update","confidence":0.9,
            "audience":"invalid","occurred_at":"2026-01-01T00:00:00Z",
            "evidence":[{"url":"https://example.com/evidence","quote":"update"}],
            "provenance":{"producer":"acme.feed","run_id":"run-1","method":"observed"}
        }]
    });
    assert!(serde_json::from_value::<SignalBatchIn>(body.clone()).is_err());
    body["signals"][0]["audience"] = json!("persona_eligible");
    api_error(writer.post::<_, SignalBatchOut>("/intel/signals", &body).await, 422, "audience_exceeds_ceiling");
    body["signals"][0]["links"] = json!([{"target_type":"news","target_id":Uuid::new_v4(),"relation":"about"}]);
    body["signals"][0]["audience"] = json!("company_internal");
    api_error(writer.post::<_, SignalBatchOut>("/intel/signals", &body).await, 404, "not_found");
    body["signals"][0]["links"] = json!([
        {"target_type":"news","target_id":news.id,"relation":"about"},
        {"target_type":"news","target_id":news.id,"relation":"derived_from"}
    ]);
    body["signals"][0]["audience"] = Value::Null;
    let mut input: SignalBatchIn = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(input.signals[0].audience, None);
    assert_eq!(serde_json::to_value(&input).unwrap()["signals"][0].get("audience"), Some(&Value::Null));
    body["signals"][0].as_object_mut().unwrap().remove("audience");
    assert_eq!(serde_json::from_value::<SignalBatchIn>(body).unwrap(), input);
    let ack = writer.intel_ingest(&input, Some("retry")).await.unwrap();
    assert!(matches!(ack, SignalBatchOut::Accepted { accepted_count: 1, .. }));
    assert_eq!(writer.intel_ingest(&input, Some("retry")).await.unwrap(), ack);
    let first_input = input.clone();
    api_error(reader.intel_ingest(&input, Some("retry")).await, 403, "intel_write_denied");
    input.signals[0].title = "Different update".into();
    api_error(writer.intel_ingest(&input, Some("retry")).await, 409, "idempotency_conflict");
    input.signals[0].title = "Updated quarterly report".into();
    input.signals[0].facts = json!({"large_number": 1e16, "zero": -0.0, "ratio": 0.123456789});
    input.signals[0].confidence = -0.0;
    input.signals[0].project_id = Some(project);
    input.signals[0].persona_id = Some(persona);
    writer.intel_ingest(&input, None).await.unwrap();
    let signal = op.intel_signals(&IntelSignalQuery { q: Some("quarterly".into()), ..Default::default() }).await.unwrap().items.remove(0);
    let id = signal.id;
    assert_eq!(signal.company_id, "acme");
    assert_eq!(signal.base_audience, Audience::CompanyInternal);
    assert_eq!(signal.severity, Severity::Warning);
    let detail = op.intel_signal(id).await.unwrap();
    assert_eq!(detail.revisions.len(), 2);
    assert_eq!(detail.signal.project_id, Some(project));
    assert_eq!(detail.signal.persona_id, Some(persona));
    assert_eq!(detail.signal.external_id.as_deref(), Some("event-1"));
    assert_eq!(detail.links.len(), 2);
    for link in &detail.links {
        assert_eq!(link.target_type, LinkTargetType::News);
        assert_eq!(link.target_id, news.id.to_string());
    }
    assert!(detail.links.iter().any(|link| link.relation == LinkRelation::About));
    assert!(detail.links.iter().any(|link| link.relation == LinkRelation::DerivedFrom));
    assert!(!op.intel_sources().await.unwrap()[0].stale);
    let public_status = writer.get::<Vec<Value>>("/intel/sources").await.unwrap().remove(0);
    assert_eq!(public_status["last_error_code"], "upstream");
    assert!(public_status.get("last_error_detail").is_none() && public_status.get("watermark").is_none());
    api_error(reader.intel_signal(id).await, 404, "not_found");
    let review = signal.review.unwrap();
    let clearance = SignalClearanceIn {
        audience: Audience::PublicCleared,
        revision: review.revision,
        policy_version: review.policy_version,
        source_default_audience: signal.source_default_audience,
        projection_sha256: review.projection_sha256,
        rationale: "Approved for public release".into(),
    };
    api_error(reader.intel_clear(id, &clearance).await, 404, "not_found");
    let mut stale = clearance.clone();
    stale.revision += 1;
    api_error(op.intel_clear(id, &stale).await, 409, "stale_revision");
    let stale = SignalClearanceIn { source_default_audience: Audience::PersonaEligible, ..clearance.clone() };
    api_error(op.intel_clear(id, &stale).await, 409, "stale_revision");
    op.intel_clear(id, &clearance).await.unwrap();
    assert_eq!(reader.today().await.unwrap().intelligence[0].subject_id, id);
    assert!(ordinary.today().await.unwrap().intelligence.is_empty());
    let decided = op
        .intel_decide(
            id,
            &SignalDecisionIn { action: SignalDecisionAction::Dismiss, until: None, note: Some("private review note".into()) },
        )
        .await
        .unwrap();
    assert_eq!(decided.state, SignalState::Dismissed);
    op.intel_decide(
        id,
        &SignalDecisionIn {
            action: SignalDecisionAction::Snooze,
            until: Some(chrono::Utc::now() + chrono::TimeDelta::hours(1)),
            note: None,
        },
    )
    .await
    .unwrap();
    let raw = reader.get::<Value>(&format!("/intel/signals/{id}")).await.unwrap();
    let page = reader.get::<Value>("/intel/signals?state=new").await.unwrap();
    for field in ["external_id", "snoozed_until", "operator_note", "persona_id", "project_id", "owner"] {
        assert!(raw["signal"].get(field).is_none(), "unexpected public field: {field}");
    }
    assert_eq!(page["items"], json!([raw["signal"]]));
    assert_eq!(raw["signal"]["state"], "new");
    assert_eq!(raw["signal"]["base_audience"], "public_cleared");
    assert_eq!(raw["signal"]["source_default_audience"], "public_cleared");
    assert_eq!(raw["signal"]["cleared_audience"], "public_cleared");
    assert_eq!(raw["links"], json!([]));
    assert_eq!(raw["revisions"], json!([]));
    let mut public_fields = raw["signal"].as_object().unwrap().clone();
    for field in
        ["id", "company_id", "source_id", "state", "base_audience", "effective_audience", "source_default_audience", "cleared_audience"]
    {
        public_fields.remove(field).unwrap();
    }
    let expected_hash =
        domain::hash::sha256_hex(domain::hash::canonical_json(&json!({"v":"intel-projection-v2","projection":public_fields})).as_bytes());
    assert_eq!(clearance.projection_sha256, expected_hash);
    assert!(reader.intel_signal(id).await.unwrap().signal.review.is_none());
    assert!(reader.intel_signals(&IntelSignalQuery { project: Some(project), ..Default::default() }).await.unwrap().items.is_empty());
    assert!(reader.intel_signals(&IntelSignalQuery { persona: Some(persona), ..Default::default() }).await.unwrap().items.is_empty());
    assert_eq!(reader.today().await.unwrap().intelligence[0].subject_id, id);

    input.signals[0].audience = Some(Audience::OperatorOnly);
    writer.intel_ingest(&input, None).await.unwrap();
    api_error(op.intel_clear(id, &clearance).await, 409, "stale_revision");
    api_error(writer.intel_ingest(&input, None).await, 429, "source_budget_exceeded");
    assert_eq!(writer.intel_ingest(&first_input, Some("retry")).await.unwrap(), ack);
    api_error(reader.get::<Value>(&format!("/history/signal/{id}")).await, 404, "not_found");

    server.abort();
    let _ = server.await;
    domain::sqlx::query(&format!("drop database {name} with (force)")).execute(&admin).await.unwrap();
    std::fs::remove_file(temp.join("intel.json")).unwrap();
    std::fs::remove_dir_all(temp).unwrap();
}
