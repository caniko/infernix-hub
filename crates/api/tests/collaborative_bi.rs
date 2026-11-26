use chrono::{Duration, Utc};
use contracts::{
    client::{ClientError, HubClient},
    *,
};
use domain::sqlx::{self, PgPool};
use fleetix_hubd::{
    AppState,
    bootstrap::{self, Settings},
};
use serde_json::{Value, json};
use uuid::Uuid;

fn denied<T: std::fmt::Debug>(result: Result<T, ClientError>, status: u16) {
    assert!(matches!(result, Err(ClientError::Api { status: got, .. }) if got == status), "{result:?}");
}

fn serve(listener: tokio::net::TcpListener, state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { axum::serve(listener, fleetix_hubd::router(state)).await.unwrap() })
}

fn batch(source: &str, external_id: &str, title: &str, at: &str, links: Value) -> SignalBatchIn {
    serde_json::from_value(json!({
        "source":source,
        "run":{"run_id":"fixture","status":"ok","finished_at":at},
        "signals":[{
            "external_id":external_id,"kind":"change","severity":"warning","title":title,"occurred_at":at,
            "confidence":1.0,"evidence":[{"url":"https://example.org/report","quote":"reviewed fixture"}],
            "provenance":{"producer":"fixture","run_id":"fixture","method":"observed"},"links":links
        }]
    }))
    .unwrap()
}

fn signal_id(receipt: SignalBatchOut) -> Uuid {
    let SignalBatchOut::Receipts { receipts, .. } = receipt else { panic!("steward did not receive item receipts") };
    assert_eq!(receipts.len(), 1);
    receipts[0].id
}

fn clearance(signal: &IntelSignal, audience: Audience) -> SignalClearanceIn {
    let review = signal.review.as_ref().unwrap();
    SignalClearanceIn {
        audience,
        revision: review.revision,
        policy_version: review.policy_version,
        source_default_audience: signal.source_default_audience,
        projection_sha256: review.projection_sha256.clone(),
        rationale: "Reviewed the exact redacted projection.".into(),
    }
}

async fn wait_for_row_locks(db: &PgPool, query_part: &str, expected: i64) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity
                 where datname=current_database() and wait_event_type='Lock' and strpos(query, $1)>0",
            )
            .bind(query_part)
            .fetch_one(db)
            .await
            .unwrap();
            if count >= expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {expected} blocked queries containing {query_part}"));
}

#[tokio::test]
async fn collaborative_bi_http() {
    let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let admin = PgPool::connect(&base).await.unwrap();
    let name = format!("collaborative_{}", Uuid::new_v4().simple());
    domain::sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let (head, query) = base.split_once('?').unwrap_or((&base, ""));
    let db_url = format!("{}/{name}?{query}", head.rsplit_once('/').unwrap().0);
    let legacy_db = PgPool::connect(&db_url).await.unwrap();
    sqlx::migrate::Migrator {
        migrations: domain::MIGRATOR.iter().filter(|m| m.version < 23).cloned().collect::<Vec<_>>().into(),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&legacy_db)
    .await
    .unwrap();
    let legacy_account: Uuid = sqlx::query_scalar(
        "insert into mail_accounts (owner,label,provider,transport) values ('legacy','legacy','imap','client_push') returning id",
    )
    .fetch_one(&legacy_db)
    .await
    .unwrap();
    let legacy_signal = Uuid::new_v4();
    sqlx::query("insert into intel_sources (id, company_id) values ('beloda.feed', 'beloda')").execute(&legacy_db).await.unwrap();
    sqlx::query(
        "insert into intel_signals (id, company_id, source_id, external_id, kind, sticky_min_audience, cleared_audience,
            cleared_base_audience, cleared_source_default_audience, cleared_revision, cleared_policy_version, cleared_sha256,
            severity, title, evidence, confidence, provenance, occurred_at, projection_sha256)
         values ($1, 'beloda', 'beloda.feed', 'legacy-cleared', 'change', 'company_internal', 'company_internal',
            'company_internal', 'company_internal', 1, 1, 'legacy', 'info', 'Legacy cleared signal',
            '[{\"url\":\"https://example.org/report\",\"quote\":\"legacy\"}]', 1,
            '{\"producer\":\"fixture\",\"run_id\":\"legacy\",\"method\":\"observed\"}', now(), 'legacy')",
    )
    .bind(legacy_signal)
    .execute(&legacy_db)
    .await
    .unwrap();
    legacy_db.close().await;
    let temp = std::env::temp_dir().join(&name);
    std::fs::create_dir(&temp).unwrap();
    let config = temp.join("intel.json");
    let policy = |company, audience| {
        json!({
            "company_id":company,"default_audience":audience,"kinds":["change"],
            "stale_after_secs":3600,"max_signals_per_day":100
        })
    };
    let mut cfg = json!({
        "sources":{
            "beloda.feed":policy("beloda","company_internal"),
            "phemus.feed":policy("phemus","company_internal"),
            "phemus.findings":policy("phemus","operator_only"),
            "phemus.drift":policy("phemus","operator_only"),
            "other.feed":policy("outside","company_internal")
        },
        "grants":{
            "dejana":{"role":"member","companies":["beloda","phemus","sorrel"],"max_audience":"company_internal",
                "can_review":true,"clear_audiences":["company_internal"],"can_manage_opportunities":true,
                "write_sources":["beloda.feed","phemus.feed"],"monitor_sources":["phemus.findings","phemus.drift"]},
            "reader":{"role":"member","company_id":"beloda","max_audience":"company_internal","write_sources":["beloda.feed"]},
            "visitor":{"role":"member","company_id":"outside","max_audience":"company_internal","can_review":true,
                "clear_audiences":["company_internal"]},
            "phemus-agent":{"role":"agent","company_id":"phemus","max_audience":"company_internal"}
        }
    });
    std::fs::write(&config, cfg.to_string()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut settings = Settings::fixture(&db_url, &listener.local_addr().unwrap().to_string());
    settings.asset_dir = temp.join("assets");
    settings.company_id = Some("studio".into());
    settings.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
    settings.seed_file = Some(settings.registry_dir.join("hub.fixture.json"));
    settings.intel_config = Some(config.clone());
    let owners = temp.join("owners.json");
    std::fs::write(&owners, r#"{"can":"can","dejana":"dejana-mail","reader":"reader","visitor":"visitor"}"#).unwrap();
    settings.principal_owners_config = Some(owners);
    let export = temp.join("org-export");
    std::fs::create_dir(&export).unwrap();
    std::fs::write(
        export.join("invoice.eml"),
        b"From: accounts@example.org\r\nSubject: Invoice BEL_SHARED_ORG\r\n\r\nPlease pay invoice 345.",
    )
    .unwrap();
    let private_export = temp.join("can-export");
    std::fs::create_dir(&private_export).unwrap();
    let mail_config = temp.join("mail.json");
    std::fs::write(&mail_config, json!({"accounts":[
        {"owner":"org","label":"beloda-shared","provider":"export_dir","transport":"export_dir","visibility":"shared","export_dir":export},
        {"owner":"can","label":"can-export","provider":"export_dir","transport":"export_dir","export_dir":private_export}
    ]}).to_string()).unwrap();
    settings.mail_config = Some(mail_config);
    let (state, _) = bootstrap::build(&settings).await.unwrap();
    for (name, role) in [
        ("can", Role::Operator),
        ("dejana", Role::Member),
        ("reader", Role::Member),
        ("visitor", Role::Member),
        ("phemus-agent", Role::Agent),
        ("legacy-agent", Role::Agent),
    ] {
        domain::system::issue_token(&state.hub.db, name, role, Some(format!("{name}-token"))).await.unwrap();
    }
    let db = state.hub.db.clone();
    assert_eq!(
        sqlx::query_scalar::<_, String>("select visibility from mail_accounts where id=$1")
            .bind(legacy_account)
            .fetch_one(&db)
            .await
            .unwrap(),
        "private"
    );
    let wrong_owner = sqlx::query("insert into mail_messages (owner,account_id,content_sha256,thread_key) values ('can',$1,'test','test')")
        .bind(legacy_account)
        .execute(&db)
        .await
        .unwrap_err();
    assert_eq!(wrong_owner.as_database_error().unwrap().code().as_deref(), Some("23503"));
    let server = serve(listener, state.clone());
    let can = HubClient::new(&settings.public_url, "can-token");
    let dejana = HubClient::new(&settings.public_url, "dejana-token");
    let reader = HubClient::new(&settings.public_url, "reader-token");
    let visitor = HubClient::new(&settings.public_url, "visitor-token");
    let agent = HubClient::new(&settings.public_url, "phemus-agent-token");
    let legacy_agent = HubClient::new(&settings.public_url, "legacy-agent-token");
    let legacy = can.intel_signal(legacy_signal).await.unwrap().signal;
    assert!(legacy.clearance_generation >= 1);
    assert!(matches!(
        can.intel_clear(legacy_signal, &clearance(&legacy, Audience::CompanyInternal)).await,
        Err(ClientError::Api { status: 409, error }) if error.code == "stale_revision"
    ));
    let status: Value = dejana.get("/status").await.unwrap();
    assert_eq!(status["principal"]["name"], "dejana");
    assert_eq!(status["integrations"], json!([]));
    assert_eq!(status["feeds"], json!([]));
    assert_eq!(status["capabilities"]["intel_review"], true);
    assert_eq!(status["capabilities"]["intel_clear"], true);
    assert_eq!(status["capabilities"]["intel_clear_audiences"], json!(["company_internal"]));
    assert_eq!(status["capabilities"]["opportunities_manage"], true);
    denied(can.get::<Value>("/notifications?owner=dejana").await, 403);
    denied(dejana.get::<Value>("/fleet/status").await, 403);
    denied(dejana.get::<Value>("/secrets").await, 403);
    assert_eq!(dejana.intel_sources().await.unwrap().iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["beloda.feed", "phemus.feed"]);
    assert!(!reader.get::<Value>("/status").await.unwrap()["capabilities"]["intel_review"].as_bool().unwrap());

    let due = Utc::now() + Duration::days(5);
    let opportunity = |company| {
        json!({
            "company_id":company,"source":"collaborative","external_id":"call","kind":"grant",
            "title":format!("{company} funding call"),"funder":"Example","url":"https://example.org/call",
            "deadline":due,"evidence":[{"url":"https://example.org/call","quote":"open"}]
        })
    };
    let opp: Opportunity = dejana.post("/ingest/opportunities", &opportunity("phemus")).await.unwrap();
    let outside: Opportunity = can.post("/ingest/opportunities", &opportunity("outside")).await.unwrap();
    let decide_opp = |state| json!({"state":state});
    for (client, state) in [(&can, "shortlisted"), (&dejana, "drafting")] {
        let row: Opportunity = client.post(&format!("/opportunities/{}/decision", opp.id), &decide_opp(state)).await.unwrap();
        assert_eq!(row.state.as_str(), state);
    }
    can.post::<_, Opportunity>(&format!("/opportunities/{}/decision", outside.id), &decide_opp("shortlisted")).await.unwrap();
    assert_eq!(dejana.opportunity(opp.id).await.unwrap().company_id, "phemus");
    assert_eq!(dejana.opportunities(None, None).await.unwrap().iter().map(|o| o.id).collect::<Vec<_>>(), [opp.id]);
    denied(dejana.opportunity(outside.id).await, 404);
    denied(dejana.post::<_, Value>(&format!("/opportunities/{}/decision", outside.id), &decide_opp("rejected")).await, 404);
    denied(dejana.post::<_, Value>("/ingest/opportunities", &opportunity("outside")).await, 403);
    denied(reader.opportunities(None, None).await, 403);
    denied(agent.post::<_, Opportunity>("/ingest/opportunities", &opportunity("studio")).await, 403);
    let studio: Opportunity = legacy_agent.post("/ingest/opportunities", &opportunity("studio")).await.unwrap();
    let mut overwrite = opportunity("studio");
    overwrite["title"] = json!("Out of scope overwrite");
    denied(agent.post::<_, Opportunity>("/ingest/opportunities", &overwrite).await, 403);
    assert_eq!(can.opportunity(studio.id).await.unwrap().title, studio.title);
    can.post::<_, Opportunity>(&format!("/opportunities/{}/decision", studio.id), &decide_opp("shortlisted")).await.unwrap();
    denied(agent.post::<_, Value>(&format!("/opportunities/{}/decision", studio.id), &decide_opp("rejected")).await, 403);
    for (client, visible, hidden) in [(&agent, opp.id, studio.id), (&legacy_agent, studio.id, opp.id)] {
        assert_eq!(client.opportunities(None, None).await.unwrap().iter().map(|o| o.id).collect::<Vec<_>>(), [visible]);
        assert_eq!(client.opportunity(visible).await.unwrap().id, visible);
        denied(client.opportunity(hidden).await, 404);
        denied(client.get::<Value>(&format!("/history/opportunity/{hidden}")).await, 404);
        assert_eq!(client.today().await.unwrap().opportunities.iter().map(|o| o.subject_id).collect::<Vec<_>>(), [visible]);
    }
    let announcements = domain::notifications::today_raises(&can.today().await.unwrap());
    domain::notifications::sync(&state.hub, "today", announcements).await.unwrap();
    for (client, allowed) in [(&agent, opp.id), (&legacy_agent, studio.id)] {
        for url in ["/notifications", "/notifications?after=0"] {
            let notes: NotificationPage = client.get(url).await.unwrap();
            assert_eq!(
                notes
                    .items
                    .iter()
                    .filter(|n| n.subject_type.as_deref() == Some("opportunity"))
                    .filter_map(|n| n.subject_id.as_deref())
                    .collect::<Vec<_>>(),
                [allowed.to_string()]
            );
        }
    }
    // Keep subsequent steward Today assertions focused on the Phemus opportunity.
    can.post::<_, Opportunity>(&format!("/opportunities/{}/decision", studio.id), &decide_opp("rejected")).await.unwrap();
    let project = Uuid::new_v4();
    sqlx::query("insert into projects (id,company_id,name,kind) values ($1,'sorrel','Sorrel','solo')")
        .bind(project)
        .execute(&db)
        .await
        .unwrap();
    let links = json!([
        {"target_type":"opportunity","target_id":opp.id,"relation":"about"},
        {"target_type":"project","target_id":project,"relation":"about"}
    ]);
    let mut can_batch = batch("beloda.feed", "can", "Shared can signal", "2026-11-01T00:00:00Z", links.clone());
    let dejana_batch = batch("phemus.feed", "dejana", "Shared dejana signal", "2026-11-02T00:00:00Z", links);
    let can_id = signal_id(can.intel_ingest(&can_batch, None).await.unwrap());
    let dejana_id = signal_id(dejana.intel_ingest(&dejana_batch, Some("dejana-ingest")).await.unwrap());
    assert_eq!(dejana_id, signal_id(dejana.intel_ingest(&dejana_batch, Some("dejana-ingest")).await.unwrap()));
    can_batch.signals[0].summary = "Second can revision".into();
    assert_eq!(signal_id(can.intel_ingest(&can_batch, None).await.unwrap()), can_id);
    let ack: SignalDecisionIn = serde_json::from_value(json!({"action":"acknowledge","note":"peer reviewed"})).unwrap();
    let member_detail = dejana.intel_signal(can_id).await.unwrap();
    assert_eq!(member_detail.revisions.len(), 2);
    assert_eq!(member_detail.links.len(), 2);
    assert_eq!(can.intel_signal(dejana_id).await.unwrap().links.len(), 2);
    let hidden_link = batch(
        "beloda.feed",
        "denied-link",
        "Out of scope link",
        "2026-11-03T00:00:00Z",
        json!([{"target_type":"opportunity","target_id":outside.id,"relation":"about"}]),
    );
    denied(dejana.intel_ingest(&hidden_link, None).await, 404);
    for (client, id) in [(&can, dejana_id), (&dejana, can_id)] {
        let signal = client.intel_decide(id, &ack).await.unwrap();
        assert_eq!(signal.state, SignalState::Acknowledged);
        let detail = client.intel_signal(id).await.unwrap();
        assert_eq!(detail.signal.operator_note.as_deref(), Some("peer reviewed"));
        assert!(!detail.revisions.is_empty());
        assert_eq!(
            client.intel_clear(id, &clearance(&detail.signal, Audience::CompanyInternal)).await.unwrap().cleared_audience,
            Some(Audience::CompanyInternal)
        );
    }
    let plain = reader.intel_signal(can_id).await.unwrap();
    assert!(plain.signal.operator_note.is_none() && plain.revisions.is_empty() && plain.links.is_empty());
    denied(reader.intel_decide(can_id, &ack).await, 403);
    denied(reader.intel_clear(can_id, &clearance(&member_detail.signal, Audience::PublicCleared)).await, 403);
    denied(dejana.intel_clear(can_id, &clearance(&member_detail.signal, Audience::OperatorOnly)).await, 403);
    assert!(matches!(
        reader.intel_ingest(&batch("beloda.feed", "blind", "Limited writer", "2026-11-01T12:00:00Z", json!([])), None).await.unwrap(),
        SignalBatchOut::Accepted { accepted_count: 1, .. }
    ));
    let current = dejana.intel_signal(can_id).await.unwrap();
    denied(dejana.intel_clear(can_id, &clearance(&current.signal, Audience::PublicCleared)).await, 403);
    cfg["grants"]["dejana"]["clear_audiences"] = json!(["company_internal", "public_cleared"]);
    let mut public_state = state.clone();
    std::sync::Arc::make_mut(&mut public_state.hub.config).intel = serde_json::from_value(cfg.clone()).unwrap();
    public_state.hub.config.intel.validate().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public_steward = HubClient::new(format!("http://{}", listener.local_addr().unwrap()), "dejana-token");
    let public_server = serve(listener, public_state);
    let reviewed = clearance(&current.signal, Audience::PublicCleared);
    assert!(matches!(public_steward.intel_clear(can_id, &reviewed).await,
        Err(ClientError::Api { status: 409, error }) if error.code == "stale_revision"));
    assert!(matches!(public_steward.intel_clear_with_generation(can_id, &ClearSignalIn {
        reviewed: reviewed.clone(), expected_clearance_generation: Some(0),
    }).await, Err(ClientError::Api { status: 409, error }) if error.code == "stale_revision"));
    public_steward
        .intel_clear_with_generation(
            can_id,
            &ClearSignalIn { reviewed, expected_clearance_generation: Some(current.signal.clearance_generation) },
        )
        .await
        .unwrap();
    public_server.abort();
    let public = visitor.intel_signal(can_id).await.unwrap();
    assert!(
        public.signal.operator_note.is_none() && public.signal.review.is_none() && public.revisions.is_empty() && public.links.is_empty()
    );
    denied(visitor.intel_decide(can_id, &ack).await, 403);
    denied(visitor.intel_clear(can_id, &clearance(&member_detail.signal, Audience::PublicCleared)).await, 403);
    for action in ["opportunity.decided", "intel.decision", "intel.clearance"] {
        let actors: Vec<String> = sqlx::query_scalar("select distinct actor from audit_log where action=$1 order by actor")
            .bind(action)
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(actors, ["member:dejana", "operator:can"]);
    }

    let mut private_ids = Vec::new();
    for (client, canary) in [(&can, "CANARY_CAN_MAIL"), (&dejana, "CANARY_DEJANA_MAIL")] {
        let eml =
            format!("From: {canary}@example.org\r\nSubject: Invoice {canary}\r\nX-Test: {canary}\r\n\r\nPlease pay invoice {canary}.");
        let message = integrations::mail_parse::parse_eml(eml.as_bytes(), "INBOX").unwrap();
        let ingest: MailIngestResult = client
            .post(
                "/mail/ingest",
                &json!({
                    "account":"personal","provider":"export_dir","messages":[message]
                }),
            )
            .await
            .unwrap();
        assert_eq!(ingest.inserted, 1);
        let messages = client.list_mail_messages(&MailSearchQuery { q: Some(canary.into()), ..Default::default() }).await.unwrap();
        assert_eq!(messages.len(), 1);
        private_ids.push(messages[0].id);
        assert!(client.get_mail_message(messages[0].id).await.unwrap().body_text.contains(canary));
    }
    let synced = dejana.sync_mail().await.unwrap();
    assert_eq!(synced.iter().find(|a| a.visibility == MailVisibility::Shared).unwrap().status, "ok");
    assert_eq!(
        sqlx::query_scalar::<_, String>("select status from mail_accounts where label='can-export'").fetch_one(&db).await.unwrap(),
        "idle"
    );
    assert_eq!(can.sync_mail().await.unwrap().iter().find(|a| a.label == "can-export").unwrap().status, "ok");
    let shared = can.list_mail_messages(&MailSearchQuery { q: Some("BEL_SHARED_ORG".into()), ..Default::default() }).await.unwrap();
    assert_eq!(shared.len(), 1);
    let shared_id = shared[0].id;
    for (client, opposite_id, opposite_canary) in
        [(&can, private_ids[1], "CANARY_DEJANA_MAIL"), (&dejana, private_ids[0], "CANARY_CAN_MAIL")]
    {
        assert!(
            client.list_mail_messages(&MailSearchQuery { q: Some(opposite_canary.into()), ..Default::default() }).await.unwrap().is_empty()
        );
        denied(client.get_mail_message(opposite_id).await, 404);
        denied(client.post::<_, Value>(&format!("/mail/messages/{opposite_id}/triage"), &json!({"classification":"legal"})).await, 404);
        assert!(client.get_mail_message(shared_id).await.unwrap().message.subject.contains("BEL_SHARED_ORG"));
        let triage: MailMessageDetail =
            client.post(&format!("/mail/messages/{shared_id}/triage"), &json!({"classification":"legal"})).await.unwrap();
        assert_eq!(triage.message.classification, "legal");
        let overview = client.mail_bi_overview().await.unwrap();
        assert_eq!(overview.total_count, 2);
        assert!(!serde_json::to_string(&overview).unwrap().to_lowercase().contains(&opposite_canary.to_lowercase()));
        let now = Utc::now();
        let digest = client.create_mail_bi_digest("test", now - Duration::days(2), now + Duration::days(1)).await.unwrap();
        assert_eq!(digest.metrics_json["total_count"], 1);
        assert!(
            !serde_json::to_string(&client.list_mail_bi_digests().await.unwrap())
                .unwrap()
                .to_lowercase()
                .contains(&opposite_canary.to_lowercase())
        );
    }
    let owned = batch(
        "beloda.feed",
        "owned",
        "Private linked signal",
        "2026-11-04T00:00:00Z",
        json!([{"target_type":"mail_message","target_id":private_ids[1],"relation":"derived_from"}]),
    );
    assert!(matches!(dejana.intel_ingest(&owned, None).await.unwrap(), SignalBatchOut::Accepted { accepted_count: 1, .. }));
    let owned_id: Uuid =
        sqlx::query_scalar("select id from intel_signals where owner='dejana-mail' and external_id='owned'").fetch_one(&db).await.unwrap();
    denied(dejana.intel_signal(owned_id).await, 404);
    denied(dejana.intel_decide(owned_id, &ack).await, 404);
    denied(can.intel_signal(owned_id).await, 404);
    let mut cross_link = owned.clone();
    cross_link.signals[0].links[0].target_id = shared_id.to_string();
    denied(dejana.intel_ingest(&cross_link, None).await, 404);

    let promotion = |id, key| {
        json!({
            "mail_message_id":id,"source_id":"beloda.feed","idempotency_key":key,"kind":"change","severity":"warning",
            "title":"Payments update","summary":"A payment needs approval.","facts":{"approvals":1},
            "evidence":[{"url":"https://example.org/approval","quote":"Approval needed"}]
        })
    };
    let path = "/intel/promotions/from-mail";
    let request = promotion(private_ids[0], "redact-can");
    denied(dejana.post::<_, Value>(path, &request).await, 404);
    denied(can.post::<_, Value>(path, &promotion(private_ids[1], "cross")).await, 404);
    let mut forbidden_link = request.clone();
    forbidden_link["links"] = json!([{"target_type":"mail_message","target_id":private_ids[0],"relation":"derived_from"}]);
    denied(can.post::<_, Value>(path, &forbidden_link).await, 422);
    let mut wrong_source = promotion(shared_id, "wrong-source");
    wrong_source["source_id"] = json!("phemus.findings");
    denied(dejana.post::<_, Value>(path, &wrong_source).await, 403);
    for forbidden_key in [
        "body_text",
        "BODY_HTML",
        "raw_path",
        "message_id_header",
        "headers",
        "raw_body",
        "raw_headers",
        "eml_path",
        "mail_message_id",
        "raw_eml",
        "rfc822",
        "attachments",
    ] {
        let mut bad = request.clone();
        bad["facts"] = json!({"nested":[{(forbidden_key):"redacted?"}]});
        denied(can.post::<_, Value>(path, &bad).await, 400);
    }
    let mut bad = request.clone();
    bad["facts"] = json!({"nested":[{"value":format!("Message {}", private_ids[0])}]});
    denied(can.post::<_, Value>(path, &bad).await, 400);
    bad["facts"] = json!({});
    bad["evidence"][0]["url"] = json!(format!("https://example.org/{}", private_ids[0]));
    denied(can.post::<_, Value>(path, &bad).await, 400);
    bad["evidence"] = json!([{"url":"https://example.org","quote":"redacted","nested":[{"raw_body":"private"}]}]);
    denied(can.post::<_, Value>(path, &bad).await, 400);
    let compact = private_ids[0].simple().to_string().to_ascii_uppercase();
    for field in ["facts", "evidence", "title", "summary"] {
        let mut bad = request.clone();
        bad[field] = match field {
            "facts" => json!({"reference":compact}),
            "evidence" => json!([{"url":"https://example.org","quote":compact}]),
            _ => json!(format!("Reference: {compact}")),
        };
        denied(can.post::<_, Value>(path, &bad).await, 400);
    }
    let mut fail = batch("beloda.feed", "unused", "unused", "2026-11-04T00:00:00Z", json!([]));
    fail.signals.clear();
    fail.run = serde_json::from_value(json!({"run_id":"failure","status":"failed","finished_at":due,
        "error_code":"offline","error_detail":"feed unavailable","watermark":{"cursor":42}}))
    .unwrap();
    can.intel_ingest(&fail, None).await.unwrap();
    let feed = |sources: Vec<IntelSourceStatus>| sources.into_iter().find(|s| s.id == "beloda.feed").unwrap();
    let before = feed(can.intel_sources().await.unwrap());
    let mut request = request;
    request["external_id"] = json!(private_ids[0]);
    let promoted: SignalBatchOut = can.post(path, &request).await.unwrap();
    assert_eq!(can.post::<_, SignalBatchOut>(path, &request).await.unwrap(), promoted);
    let mut changed = request;
    changed["summary"] = json!("Changed intent");
    denied(can.post::<_, Value>(path, &changed).await, 409);
    let promoted_id = signal_id(promoted);
    let stored_external_id = can.intel_signal(promoted_id).await.unwrap().signal.external_id.unwrap();
    for external_id in ["PrOmO:reserved", &stored_external_id] {
        let ordinary = batch("beloda.feed", external_id, "Must not overwrite a promotion", "2026-11-04T00:00:00Z", json!([]));
        denied(can.intel_ingest(&ordinary, None).await, 400);
    }
    let peer_request = promotion(private_ids[1], "redact-can");
    let peer: SignalBatchOut = dejana.post(path, &peer_request).await.unwrap();
    assert_eq!(dejana.post::<_, SignalBatchOut>(path, &peer_request).await.unwrap(), peer);
    domain::system::issue_token(&db, "dejana", Role::Operator, Some("dejana-token".into())).await.unwrap();
    assert_eq!(dejana.post::<_, SignalBatchOut>(path, &peer_request).await.unwrap(), peer);
    domain::system::issue_token(&db, "dejana", Role::Member, Some("dejana-token".into())).await.unwrap();
    let reverse_id = signal_id(peer);
    assert_ne!(promoted_id, reverse_id);
    let shared_promotion = signal_id(dejana.post(path, &promotion(shared_id, "redact-org")).await.unwrap());
    assert_eq!(feed(can.intel_sources().await.unwrap()), before);
    for (client, id, actor) in
        [(&dejana, promoted_id, "operator:can"), (&can, reverse_id, "member:dejana"), (&can, shared_promotion, "member:dejana")]
    {
        let detail = client.intel_signal(id).await.unwrap();
        assert_eq!(detail.signal.summary, "A payment needs approval.");
        assert_eq!(detail.signal.provenance.promoted_by.as_deref(), Some(actor));
        assert_eq!(detail.signal.provenance.promoted_from.as_deref(), Some("redacted_mail"));
        assert!(detail.links.is_empty());
        let full = serde_json::to_string(&detail).unwrap();
        assert!(!full.contains("CANARY_") && !full.contains(&private_ids[0].to_string()) && !full.contains(&private_ids[1].to_string()));
        assert!(
            sqlx::query_scalar::<_, bool>("select owner is null from intel_signals where id=$1").bind(id).fetch_one(&db).await.unwrap()
        );
    }

    settings.mail_config = None;
    let (standalone_worker, _) = bootstrap::build_worker(&settings).await.unwrap();
    drop(standalone_worker);
    assert!(dejana.list_mail_accounts().await.unwrap().iter().any(|account| account.label == "beloda-shared"));
    assert!(dejana.get_mail_message(shared_id).await.is_ok());
    let (restarted, _) = bootstrap::build(&settings).await.unwrap();
    drop(restarted);
    assert!(dejana.list_mail_accounts().await.unwrap().iter().all(|account| account.label != "beloda-shared"));
    assert_eq!(dejana.mail_bi_overview().await.unwrap().total_count, 1);
    denied(dejana.get_mail_message(shared_id).await, 404);
    assert!(
        dejana.list_mail_messages(&MailSearchQuery { q: Some("BEL_SHARED_ORG".into()), ..Default::default() }).await.unwrap().is_empty()
    );
    denied(dejana.post::<_, Value>(path, &promotion(shared_id, "redact-org")).await, 404);
    assert_eq!(can.intel_signal(shared_promotion).await.unwrap().signal.summary, "A payment needs approval.");

    let raise = json!({"source":"bi","dedupe_key":"private","severity":"warning","title":"Private notification"});
    let note: Notification = dejana.post("/notifications", &raise).await.unwrap();
    assert_eq!(note.dedupe_key, "member:dejana:private");
    let shared_note: Notification = can
        .post("/notifications", &json!({"source":"bi-sync","dedupe_key":"shared","severity":"warning","title":"Shared notification"}))
        .await
        .unwrap();
    let sync = json!({"source":"bi-sync","items":[{"dedupe_key":"sync","severity":"warning","title":"Synced notification"}]});
    denied(can.post::<_, Value>("/notifications/sync?owner=dejana", &sync).await, 403);
    can.post::<_, SyncNotificationsOut>("/notifications/sync?owner=can", &sync).await.unwrap();
    assert!(can.get::<NotificationPage>("/notifications?owner=can").await.unwrap().items.iter().any(|n| n.dedupe_key == "member:can:sync"));
    let shared_inbox: NotificationPage = can.get("/notifications").await.unwrap();
    assert!(shared_inbox.items.iter().any(|n| n.id == shared_note.id));
    assert!(!shared_inbox.items.iter().any(|n| n.dedupe_key == "sync"));
    let renewal_id = Uuid::new_v4();
    sqlx::query("insert into renewals (id,company_id,key,title,owner) values ($1,'studio','personal','Personal renewal','dejana')")
        .bind(renewal_id)
        .execute(&db)
        .await
        .unwrap();
    domain::notifications::sync_member_renewals(&state.hub).await.unwrap();
    let personal = dejana.get::<NotificationPage>("/notifications?owner=dejana").await.unwrap();
    assert!(personal.items.iter().any(|n| n.id == note.id));
    assert!(personal.items.iter().any(|n| n.subject_id.as_deref() == Some(renewal_id.to_string().as_str())));
    assert_eq!(dejana.get::<Vec<Renewal>>("/renewals").await.unwrap().iter().map(|r| r.id).collect::<Vec<_>>(), [renewal_id]);
    let own: Notification = can.post("/notifications?owner=can", &raise).await.unwrap();
    assert_eq!(can.get::<NotificationPage>("/notifications?owner=can").await.unwrap().items[0].id, own.id);
    denied(dejana.get::<Value>("/notifications?owner=dejana-mail").await, 403);
    for owner in ["dejana", "dejana-mail"] {
        denied(can.get::<Value>(&format!("/notifications?owner={owner}")).await, 403);
        denied(can.post::<_, Value>(&format!("/notifications?owner={owner}"), &raise).await, 403);
        denied(can.post::<_, Value>(&format!("/notifications/ack?owner={owner}"), &json!({"all":true})).await, 403);
        denied(can.post::<_, Value>(&format!("/notifications/{}/snooze?owner={owner}", note.id), &json!({"until":due})).await, 403);
    }
    assert!(can.get::<Vec<HistoryEntry>>(&format!("/history/notification/{}", note.id)).await.unwrap().is_empty());

    for source in ["phemus.findings", "phemus.drift"] {
        let id = signal_id(
            can.intel_ingest(&batch(source, "secret", "HIDDEN_OPERATOR_INTEL", "2026-11-25T00:00:00Z", json!([])), None).await.unwrap(),
        );
        denied(dejana.intel_signal(id).await, 404);
        denied(dejana.get::<Value>(&format!("/history/signal/{id}")).await, 404);
        assert!(
            dejana.intel_signals(&IntelSignalQuery { source: Some(source.into()), ..Default::default() }).await.unwrap().items.is_empty()
        );
    }
    let hidden_search = IntelSignalQuery { q: Some("HIDDEN_OPERATOR_INTEL".into()), ..Default::default() };
    assert!(dejana.intel_signals(&hidden_search).await.unwrap().items.is_empty());
    let mut query = IntelSignalQuery { limit: Some(1), ..Default::default() };
    let mut seen = Vec::new();
    loop {
        let page = dejana.intel_signals(&query).await.unwrap();
        seen.extend(page.items.iter().map(|s| s.id));
        assert!(page.items.iter().all(|s| !s.title.contains("HIDDEN_OPERATOR_INTEL")));
        let Some(cursor) = page.next_cursor else { break };
        query.cursor = Some(cursor);
    }
    assert!(seen.contains(&can_id) && seen.contains(&dejana_id) && !seen.contains(&private_ids[0]));
    assert!(seen.contains(&legacy_signal));
    assert_eq!(seen.len(), 7);
    let today = dejana.today().await.unwrap();
    assert!(today.intelligence.iter().any(|item| item.subject_id == promoted_id));
    assert!(today.intelligence.iter().all(|item| !item.title.contains("HIDDEN_OPERATOR_INTEL")));
    assert_eq!(today.opportunities.iter().map(|o| o.subject_id).collect::<Vec<_>>(), [opp.id]);
    assert_eq!(Today { intelligence: vec![], opportunities: vec![], ..today }, Today::default());

    let mut secret = batch("phemus.findings", "redacted", "Private revision one", "2026-11-04T00:00:00Z", json!([]));
    secret.signals[0].facts = json!({"secret":"DO_NOT_SHARE"});
    let secret_id = signal_id(can.intel_ingest(&secret, None).await.unwrap());
    can.intel_decide(secret_id, &ack).await.unwrap();
    secret.signals[0].title = "Redacted revision two".into();
    secret.signals[0].facts = json!({});
    assert_eq!(signal_id(can.intel_ingest(&secret, None).await.unwrap()), secret_id);
    let current = can.intel_signal(secret_id).await.unwrap();
    let cleared = can.intel_clear(secret_id, &clearance(&current.signal, Audience::CompanyInternal)).await.unwrap();
    let redacted = dejana.intel_signal(secret_id).await.unwrap();
    assert_eq!(redacted.signal.title, "Redacted revision two");
    assert_eq!(redacted.signal.review.as_ref().unwrap().revision, 2);
    assert!(redacted.revisions.is_empty() && redacted.signal.operator_note.is_none());
    assert!(redacted.signal.external_id.is_none() && redacted.links.is_empty());
    assert_eq!(redacted.signal.state, SignalState::New);
    let projection = |signal: &IntelSignal| (signal.review.as_ref().unwrap().updated_at, signal.clearance_generation);
    let redacted_query = IntelSignalQuery { source: Some("phemus.findings".into()), ..Default::default() };
    assert_eq!(projection(&redacted.signal), (projection(&cleared).0, 0));
    assert_eq!(projection(&dejana.intel_signals(&redacted_query).await.unwrap().items[0]), projection(&redacted.signal));
    let decided = can
        .intel_decide(
            secret_id,
            &SignalDecisionIn { action: SignalDecisionAction::Snooze, note: Some("private follow-up".into()), until: Some(due) },
        )
        .await
        .unwrap();
    assert!(projection(&decided).0 > projection(&redacted.signal).0);
    assert_eq!(projection(&dejana.intel_signal(secret_id).await.unwrap().signal), projection(&redacted.signal));
    assert_eq!(projection(&dejana.intel_signals(&redacted_query).await.unwrap().items[0]), projection(&redacted.signal));
    denied(dejana.intel_decide(secret_id, &ack).await, 403);
    denied(dejana.intel_clear(secret_id, &clearance(&redacted.signal, Audience::CompanyInternal)).await, 403);

    let visible = batch("beloda.feed", "replay-visible", "Visible for replay", "2026-11-04T00:00:00Z", json!([]));
    let SignalBatchOut::Receipts { delivery_id, .. } = dejana.intel_ingest(&visible, Some("formerly-visible")).await.unwrap() else {
        panic!("visible delivery must have receipts")
    };
    let mut reclassified = visible.clone();
    reclassified.signals[0].audience = Some(Audience::OperatorOnly);
    can.intel_ingest(&reclassified, None).await.unwrap();
    assert_eq!(
        dejana.intel_ingest(&visible, Some("formerly-visible")).await.unwrap(),
        SignalBatchOut::Accepted { delivery_id, accepted_count: 1 }
    );
    let mut mixed = batch("beloda.feed", "mixed-visible", "Visible part", "2026-11-04T00:00:00Z", json!([]));
    let mut hidden = mixed.signals[0].clone();
    hidden.external_id = "mixed-hidden".into();
    hidden.audience = Some(Audience::OperatorOnly);
    mixed.signals.push(hidden);
    let fresh = dejana.intel_ingest(&mixed, Some("mixed")).await.unwrap();
    assert!(matches!(fresh, SignalBatchOut::Accepted { accepted_count: 2, .. }));
    assert_eq!(dejana.intel_ingest(&mixed, Some("mixed")).await.unwrap(), fresh);

    // Hold the row while actual ingest and clearance queue behind it, in that order.
    let old = clearance(&dejana.intel_signal(can_id).await.unwrap().signal, Audience::PublicCleared);
    let mut gate = db.begin().await.unwrap();
    sqlx::query("select id from intel_signals where id=$1 for update").bind(can_id).execute(&mut *gate).await.unwrap();
    can_batch.signals[0].summary = "A concurrent content edit".into();
    let update = tokio::spawn({
        let can = can.clone();
        async move { can.intel_ingest(&can_batch, None).await.unwrap() }
    });
    wait_for_row_locks(&db, "owner_namespace = $3", 1).await;
    let clear = tokio::spawn({
        let can = can.clone();
        async move { can.intel_clear(can_id, &old).await }
    });
    wait_for_row_locks(&db, "owner is null or owner = $2", 1).await;
    gate.commit().await.unwrap();
    assert_eq!(signal_id(update.await.unwrap()), can_id);
    assert!(matches!(clear.await.unwrap(), Err(ClientError::Api { status: 409, error }) if error.code == "stale_revision"));
    let revised = dejana.intel_signal(can_id).await.unwrap();
    assert_eq!(revised.revisions.len(), 3);
    assert!(revised.signal.cleared_audience.is_none());

    let mut gate = db.begin().await.unwrap();
    sqlx::query("select id from intel_signals where id=$1 for update").bind(can_id).execute(&mut *gate).await.unwrap();
    let generation = revised.signal.clearance_generation;
    let await_clearance = |client: HubClient, audience| {
        let body = ClearSignalIn { reviewed: clearance(&revised.signal, audience), expected_clearance_generation: Some(generation) };
        tokio::spawn(async move { client.intel_clear_with_generation(can_id, &body).await })
    };
    let first = await_clearance(can.clone(), Audience::PublicCleared);
    wait_for_row_locks(&db, "owner is null or owner = $2", 1).await;
    let second = await_clearance(dejana.clone(), Audience::CompanyInternal);
    wait_for_row_locks(&db, "owner is null or owner = $2", 2).await;
    gate.commit().await.unwrap();
    assert_eq!(first.await.unwrap().unwrap().clearance_generation, generation + 1);
    assert!(matches!(second.await.unwrap(), Err(ClientError::Api { status: 409, error }) if error.code == "stale_revision"));
    assert_eq!(can.intel_signal(can_id).await.unwrap().signal.cleared_audience, Some(Audience::PublicCleared));

    let mut gate = db.begin().await.unwrap();
    sqlx::query("select id from intel_signals where id=$1 for update").bind(can_id).execute(&mut *gate).await.unwrap();
    let first = tokio::spawn({
        let can = can.clone();
        let ack = ack.clone();
        async move { can.intel_decide(can_id, &ack).await.unwrap() }
    });
    wait_for_row_locks(&db, "owner is null or owner = $2", 1).await;
    let second = tokio::spawn({
        let dejana = dejana.clone();
        async move {
            dejana
                .intel_decide(can_id, &SignalDecisionIn { action: SignalDecisionAction::Dismiss, note: Some("second".into()), until: None })
                .await
                .unwrap()
        }
    });
    wait_for_row_locks(&db, "owner is null or owner = $2", 2).await;
    gate.commit().await.unwrap();
    assert_eq!(first.await.unwrap().state, SignalState::Acknowledged);
    assert_eq!(second.await.unwrap().state, SignalState::Dismissed);
    assert_eq!(dejana.intel_signal(can_id).await.unwrap().signal.operator_note.as_deref(), Some("second"));

    // Rebuild the router over the same database with current policy, as on a daemon restart.
    cfg["sources"]["beloda.feed"]["policy_version"] = json!(2);
    let reload = |cfg: &Value| {
        let mut state = state.clone();
        std::sync::Arc::make_mut(&mut state.hub.config).intel = serde_json::from_value(cfg.clone()).unwrap();
        state.hub.config.intel.validate().unwrap();
        async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = HubClient::new(format!("http://{}", listener.local_addr().unwrap()), "dejana-token");
            (client, serve(listener, state))
        }
    };
    let (changed_policy, changed_server) = reload(&cfg).await;
    assert!(matches!(changed_policy.intel_clear(can_id, &clearance(&revised.signal, Audience::PublicCleared)).await,
        Err(ClientError::Api { status: 409, error }) if error.code == "stale_revision"));
    changed_server.abort();
    cfg["sources"]["beloda.feed"]["default_audience"] = json!("operator_only");
    let (tightened, tightened_server) = reload(&cfg).await;
    denied(tightened.intel_signal(promoted_id).await, 404);
    denied(tightened.intel_clear(can_id, &clearance(&revised.signal, Audience::PublicCleared)).await, 404);
    assert!(tightened.intel_sources().await.unwrap().iter().all(|s| s.id != "beloda.feed"));
    assert!(tightened.today().await.unwrap().intelligence.iter().all(|s| s.subject_id != promoted_id));
    tightened_server.abort();
    cfg["grants"].as_object_mut().unwrap().remove("dejana");
    let (revoked, revoked_server) = reload(&cfg).await;
    denied(revoked.intel_signal(dejana_id).await, 403);
    denied(revoked.intel_ingest(&dejana_batch, None).await, 403);
    denied(revoked.opportunities(None, None).await, 403);
    assert_eq!(revoked.today().await.unwrap(), Today::default());
    assert!(!revoked.status().await.unwrap().capabilities.unwrap().intel_read);
    revoked_server.abort();

    server.abort();
    db.close().await;
    domain::sqlx::query(&format!("drop database {name} with (force)")).execute(&admin).await.unwrap();
    std::fs::remove_dir_all(temp).unwrap();
}
