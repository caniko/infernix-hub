use super::*;
use crate::intel::{IntelConfig, clear_signal, decide_signal, get_signal, ingest_batch, list_signals, list_sources};
use crate::mail_calendar_tests::{test_database, test_hub};
use contracts::*;
use serde_json::json;
use uuid::Uuid;

fn actor(name: &str, role: Role) -> Actor {
    Actor { principal: Principal { id: Uuid::new_v4(), name: name.into(), role }, correlation_id: None }
}

fn config() -> IntelConfig {
    serde_json::from_value(json!({
        "sources": {
            "acme.feed": {"company_id":"acme","default_audience":"company_internal","kinds":["change"],
                "stale_after_secs":60,"max_signals_per_day":20},
            "acme.drift": {"company_id":"acme","default_audience":"operator_only","kinds":["change"],
                "stale_after_secs":60,"max_signals_per_day":1},
            "social.trends": {"company_id":"other","default_audience":"persona_eligible","kinds":["change"],
                "stale_after_secs":60,"max_signals_per_day":20}
        },
        "grants": {
            "reader": {"company_id":"acme","role":"agent","max_audience":"company_internal"},
            "outside": {"company_id":"other","role":"agent","max_audience":"company_internal"},
            "writer": {"company_id":"acme","role":"ingest","max_audience":"persona_eligible",
                "write_sources":["acme.feed","acme.drift"],"monitor_sources":["acme.feed"]}
        }
    }))
    .unwrap()
}

fn batch(source: &str, title: &str, audience: Audience) -> SignalBatchIn {
    serde_json::from_value(json!({
        "source":source,
        "run":{"run_id":"run-1","status":"partial","finished_at":"2026-01-01T00:00:00Z",
            "watermark":{"page":4},"error_code":"unavailable","error_detail":"internal diagnostics"},
        "signals":[{
            "external_id":"event","kind":"change","severity":"warning","title":title,"summary":"Quarterly results",
            "occurred_at":"2026-01-01T00:00:00.123456789Z","audience":audience,"confidence":0.8,
            "evidence":[{"url":"https://example.com/evidence","quote":"Reported result"}],
            "subjects":[{"scheme":"domain","value":"example.com"}],
            "provenance":{"producer":source,"run_id":"run-1","method":"observed"}
        }]
    }))
    .unwrap()
}

fn first(out: SignalBatchOut) -> IngestReceipt {
    match out {
        SignalBatchOut::Receipts { mut receipts, .. } => receipts.remove(0),
        other => panic!("expected identifiable receipts, got {other:?}"),
    }
}

async fn put(hub: &Hub, actor: &Actor, batch: SignalBatchIn) -> IngestReceipt {
    first(ingest_batch(hub, actor, batch, None).await.unwrap())
}

fn clearance(signal: &IntelSignal) -> SignalClearanceIn {
    let review = signal.review.as_ref().unwrap();
    SignalClearanceIn {
        audience: Audience::PublicCleared,
        revision: review.revision,
        policy_version: review.policy_version,
        source_default_audience: signal.source_default_audience,
        projection_sha256: review.projection_sha256.clone(),
        rationale: "Approved for public release".into(),
    }
}

#[test]
fn intel_config_rejects_unsafe_grants_and_invalid_source_ids() {
    let intel = config();
    intel.validate().unwrap();
    assert_eq!(Audience::ALL.iter().map(Audience::rank).collect::<Vec<_>>(), vec![3, 2, 1, 0]);
    for bad in ["unconfigured", "social.trends"] {
        let mut bad_config = intel.clone();
        bad_config.grants.get_mut("writer").unwrap().monitor_sources.push(bad.into());
        assert!(bad_config.validate().is_err());
    }
    let mut bad_config = intel.clone();
    bad_config.grants.get_mut("reader").unwrap().max_audience = Audience::OperatorOnly;
    assert!(bad_config.validate().is_err());
    let mut bad_config = intel.clone();
    bad_config.sources.insert("Acme/feed".into(), intel.sources["acme.feed"].clone());
    assert!(bad_config.validate().is_err());
}

#[tokio::test]
async fn intel_postgres_visibility_and_revision_lifecycle() {
    let Some((db, admin, database)) = test_database().await else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let (op, peer) = (actor("operator", Role::Operator), actor("peer", Role::Operator));
    let (reader, outside, writer) = (actor("reader", Role::Agent), actor("outside", Role::Agent), actor("writer", Role::Ingest));
    let ungranted = actor("ordinary", Role::Agent);
    let mut intel = config();
    intel.grants.get_mut("reader").unwrap().principal_id = Some(reader.principal.id);
    let hub = test_hub(db.clone(), HubConfig { intel, company_id: "acme".into(), ..Default::default() });
    let a = batch("acme.feed", "Original report", Audience::CompanyInternal);
    let ack = ingest_batch(&hub, &writer, a.clone(), Some("delivery")).await.unwrap();
    assert!(matches!(ack, SignalBatchOut::Accepted { accepted_count: 1, .. }));
    let id = first(ingest_batch(&hub, &op, a.clone(), Some("delivery")).await.unwrap()).id;
    assert_eq!(ingest_batch(&hub, &writer, a.clone(), Some("delivery")).await.unwrap(), ack);
    let original = get_signal(&hub, &reader, id).await.unwrap();
    assert_eq!(original.signal.occurred_at.timestamp_subsec_nanos(), 123456000);
    assert_eq!(original.signal.base_audience, Audience::CompanyInternal);
    assert!(original.revisions.is_empty());
    for reader in [&writer, &outside] {
        assert!(matches!(get_signal(&hub, reader, id).await, Err(DomainError::NotFound(_))));
    }
    for caller in [&ungranted, &actor("reader", Role::Agent), &actor("reader", Role::Procedure)] {
        assert_eq!(get_signal(&hub, caller, id).await.unwrap_err().code(), "no_intel_grant");
    }
    assert_eq!(content::today(&hub, &reader).await.unwrap().intelligence[0].subject_id, id);
    assert!(content::today(&hub, &outside).await.unwrap().intelligence.is_empty());
    assert!(content::today(&hub, &ungranted).await.unwrap().intelligence.is_empty());
    assert_eq!(list_sources(&hub, &reader).await.unwrap().len(), 2);
    let reader_status = list_sources(&hub, &reader).await.unwrap().remove(1);
    assert_eq!(reader_status.last_error_code.as_deref(), Some("unavailable"));
    assert!(reader_status.last_error_detail.is_none() && reader_status.watermark.is_none());
    let monitor = list_sources(&hub, &writer).await.unwrap().remove(1);
    assert_eq!(monitor.watermark, Some(json!({"page":4})));
    assert!(monitor.last_error_detail.is_some());

    let (project, foreign_project) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("insert into projects (id, company_id, name, kind) values ($1, 'acme', 'Acme', 'solo'), ($2, 'other', 'Other', 'solo')")
        .bind(project)
        .bind(foreign_project)
        .execute(&db)
        .await
        .unwrap();
    let mut invalid = a.clone();
    invalid.signals[0].project_id = Some(foreign_project);
    assert_eq!(ingest_batch(&hub, &op, invalid, None).await.unwrap_err().code(), "invalid_association");
    let mut linked = a.clone();
    for relation in [LinkRelation::About, LinkRelation::DerivedFrom] {
        linked.signals[0].links.push(SignalLinkIn { target_type: LinkTargetType::Project, target_id: project.to_string(), relation });
    }
    assert_eq!(put(&hub, &op, linked).await.revision, 1);
    assert_eq!(get_signal(&hub, &reader, id).await.unwrap().links.len(), 2);
    let public = clearance(&original.signal);
    assert!(matches!(clear_signal(&hub, &outside, id, public.clone()).await, Err(DomainError::NotFound(_))));
    clear_signal(&hub, &op, id, public.clone()).await.unwrap();
    let visible_public = get_signal(&hub, &outside, id).await.unwrap();
    assert_eq!(visible_public.signal.effective_audience, Audience::PublicCleared);
    assert!(visible_public.links.is_empty());

    let mut tightening = hub.clone();
    let policy = Arc::make_mut(&mut tightening.config).intel.sources.get_mut("acme.feed").unwrap();
    policy.default_audience = Audience::OperatorOnly;
    assert!(matches!(get_signal(&tightening, &reader, id).await, Err(DomainError::NotFound(_))));
    assert!(list_signals(&tightening, &outside, Default::default()).await.unwrap().items.is_empty());
    assert!(get_signal(&tightening, &op, id).await.unwrap().signal.cleared_audience.is_none());
    Arc::make_mut(&mut tightening.config).intel.sources.get_mut("acme.feed").unwrap().policy_version += 1;
    assert_eq!(clear_signal(&tightening, &op, id, public.clone()).await.unwrap_err().code(), "stale_revision");
    Arc::make_mut(&mut tightening.config).intel.sources.remove("acme.feed");
    assert!(matches!(get_signal(&tightening, &outside, id).await, Err(DomainError::NotFound(_))));
    assert_eq!(get_signal(&tightening, &op, id).await.unwrap().signal.effective_audience, Audience::OperatorOnly);
    let mut rebound = hub.clone();
    Arc::make_mut(&mut rebound.config).intel.sources.get_mut("acme.feed").unwrap().company_id = "other".into();
    assert_eq!(ingest_batch(&rebound, &op, a.clone(), Some("delivery")).await.unwrap_err().code(), "source_company_mismatch");
    assert!(matches!(get_signal(&rebound, &outside, id).await, Err(DomainError::NotFound(_))));

    let mut b = a.clone();
    b.signals[0].title = "Updated report".into();
    assert_eq!(put(&hub, &op, b).await.revision, 2);
    assert!(matches!(get_signal(&hub, &outside, id).await, Err(DomainError::NotFound(_))));
    assert_eq!(clear_signal(&hub, &op, id, public.clone()).await.unwrap_err().code(), "stale_revision");
    assert_eq!(put(&hub, &op, a.clone()).await.revision, 3);
    let reverted = get_signal(&hub, &op, id).await.unwrap();
    assert_eq!(reverted.signal.review.unwrap().projection_sha256, public.projection_sha256);
    assert_eq!(reverted.revisions.iter().map(|r| r.revision).collect::<Vec<_>>(), vec![3, 2, 1]);
    assert_eq!(reverted.revisions[0].payload, reverted.revisions[2].payload);
    assert!(reverted.signal.cleared_audience.is_none());
    assert_eq!(ingest_batch(&hub, &writer, a.clone(), Some("delivery")).await.unwrap(), ack);

    let snooze = SignalDecisionIn { action: SignalDecisionAction::Snooze, until: None, note: None };
    assert_eq!(decide_signal(&hub, &op, id, snooze.clone()).await.unwrap_err().code(), "invalid_snooze");
    decide_signal(&hub, &op, id, SignalDecisionIn { until: Some(Utc::now() + chrono::TimeDelta::hours(1)), ..snooze }).await.unwrap();
    assert!(content::today(&hub, &reader).await.unwrap().intelligence.is_empty());
    decide_signal(
        &hub,
        &op,
        id,
        SignalDecisionIn { action: SignalDecisionAction::Dismiss, until: None, note: Some("operator note".into()) },
    )
    .await
    .unwrap();
    let mut noisy = a.clone();
    noisy.signals[0].severity = Severity::Critical;
    noisy.signals[0].provenance.method = ProvenanceMethod::Llm;
    put(&hub, &op, noisy).await;
    let dismissed = get_signal(&hub, &op, id).await.unwrap().signal;
    assert_eq!((dismissed.severity, dismissed.state), (Severity::Warning, SignalState::Dismissed));
    let mut escalation = a.clone();
    escalation.signals[0].severity = Severity::Critical;
    put(&hub, &op, escalation).await;
    let reopened = get_signal(&hub, &reader, id).await.unwrap().signal;
    assert_eq!((reopened.severity, reopened.state), (Severity::Critical, SignalState::New));
    assert!(reopened.snoozed_until.is_none() && reopened.operator_note.is_none());
    assert!(matches!(history(&db, "signal", &id.to_string()).await, Err(DomainError::NotFound(_))));
    for (source, key) in [(Some("intel"), "acme:signal"), (None, "intel:signal")] {
        let input = serde_json::from_value(json!({"source":source,"dedupe_key":key,"severity":"warning","title":"signal"})).unwrap();
        assert_eq!(notifications::post(&hub, &op, input).await.unwrap_err().code(), "reserved_key");
    }

    let mail_id: Uuid = sqlx::query_scalar(
        "with a as (insert into mail_accounts (owner,label,provider,transport) values ('operator','fixture','imap','imap') returning id)
         insert into mail_messages (account_id,owner,content_sha256,thread_key) select id,'operator','fixture','thread' from a returning id",
    ).fetch_one(&db).await.unwrap();
    let mut owned = a.clone();
    owned.signals[0].links.push(SignalLinkIn {
        target_type: LinkTargetType::MailMessage,
        target_id: mail_id.to_string(),
        relation: LinkRelation::DerivedFrom,
    });
    assert!(matches!(ingest_batch(&hub, &peer, owned.clone(), None).await, Err(DomainError::NotFound(_))));
    assert!(matches!(ingest_batch(&hub, &writer, owned.clone(), None).await, Err(DomainError::NotFound(_))));
    let owner_id = put(&hub, &op, owned).await.id;
    assert_ne!(id, owner_id);
    let owner_signal = get_signal(&hub, &op, owner_id).await.unwrap();
    assert_eq!(owner_signal.signal.base_audience, Audience::OperatorOnly);
    assert_eq!(owner_signal.links.len(), 1);
    assert_eq!(clear_signal(&hub, &op, owner_id, clearance(&owner_signal.signal)).await.unwrap_err().code(), "owner_signal_cannot_widen");
    assert!(matches!(clear_signal(&hub, &peer, owner_id, public).await, Err(DomainError::NotFound(_))));
    assert!(matches!(get_signal(&hub, &reader, owner_id).await, Err(DomainError::NotFound(_))));
    assert_eq!(content::today(&hub, &peer).await.unwrap().intelligence.len(), 1);
    let filtered =
        IntelSignalQuery { subject: Some("domain:example.com".into()), q: Some("results".into()), limit: Some(1), ..Default::default() };
    let page = list_signals(&hub, &op, filtered.clone()).await.unwrap();
    let next = list_signals(&hub, &op, IntelSignalQuery { cursor: page.next_cursor, ..filtered }).await.unwrap();
    assert_eq!(next.items.len(), 1);
    assert_ne!(page.items[0].id, next.items[0].id);
    assert!(next.next_cursor.is_none());

    let review = clearance(&reopened);
    clear_signal(&hub, &op, id, review.clone()).await.unwrap();
    let mut narrowed = a.clone();
    narrowed.signals[0].severity = reopened.severity;
    narrowed.signals[0].audience = Some(Audience::OperatorOnly);
    assert_eq!(put(&hub, &op, narrowed).await.revision, review.revision + 1);
    let tightened = get_signal(&hub, &op, id).await.unwrap();
    assert_eq!(tightened.signal.review.unwrap().projection_sha256, review.projection_sha256);
    assert_eq!(tightened.revisions[0].revision, review.revision + 1);
    assert_eq!(tightened.revisions[0].payload["sticky_min_audience"], "operator_only");
    assert!(tightened.signal.cleared_audience.is_none());
    assert_eq!(clear_signal(&hub, &op, id, review.clone()).await.unwrap_err().code(), "stale_revision");
    assert!(matches!(get_signal(&hub, &outside, id).await, Err(DomainError::NotFound(_))));
    assert_eq!(put(&hub, &op, a).await.revision, review.revision + 2);
    assert!(matches!(get_signal(&hub, &reader, id).await, Err(DomainError::NotFound(_))));

    let restricted = batch("acme.drift", "Restricted report", Audience::OperatorOnly);
    let (left, right) = tokio::join!(ingest_batch(&hub, &writer, restricted.clone(), None), ingest_batch(&hub, &writer, restricted, None));
    let outcomes = [left, right];
    assert_eq!(outcomes.iter().filter(|r| matches!(r, Ok(SignalBatchOut::Accepted { .. }))).count(), 1);
    assert_eq!(outcomes.iter().filter(|r| matches!(r, Err(DomainError::RateLimited { code: "source_budget_exceeded", .. }))).count(), 1);
    db.close().await;
    sqlx::query(&format!("drop database {database} with (force)")).execute(&admin).await.unwrap();
}

#[tokio::test]
async fn intel_postgres_clearance_revoked_by_source_default_change() {
    let Some((db, admin, database)) = test_database().await else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let (op, outside) = (actor("operator", Role::Operator), actor("outside", Role::Agent));
    let mut intel = config();
    intel.sources.get_mut("acme.feed").unwrap().default_audience = Audience::PersonaEligible;
    let mut hub = test_hub(db.clone(), HubConfig { intel, company_id: "acme".into(), ..Default::default() });
    let id = put(&hub, &op, batch("acme.feed", "Restricted report", Audience::OperatorOnly)).await.id;
    let signal = get_signal(&hub, &op, id).await.unwrap().signal;
    assert_eq!(signal.base_audience, Audience::OperatorOnly);
    let reviewed = clearance(&signal);
    assert_eq!(reviewed.source_default_audience, Audience::PersonaEligible);
    clear_signal(&hub, &op, id, reviewed.clone()).await.unwrap();
    assert_eq!(get_signal(&hub, &outside, id).await.unwrap().signal.effective_audience, Audience::PublicCleared);

    Arc::make_mut(&mut hub.config).intel.sources.get_mut("acme.feed").unwrap().default_audience = Audience::CompanyInternal;
    assert!(matches!(get_signal(&hub, &outside, id).await, Err(DomainError::NotFound(_))));
    assert!(list_signals(&hub, &outside, Default::default()).await.unwrap().items.is_empty());
    let current = get_signal(&hub, &op, id).await.unwrap().signal;
    assert_eq!(current.base_audience, signal.base_audience);
    assert_eq!(current.source_default_audience, Audience::CompanyInternal);
    assert_eq!(current.review.unwrap().policy_version, reviewed.policy_version);
    assert!(matches!(clear_signal(&hub, &op, id, reviewed).await, Err(DomainError::Conflict { code: "stale_revision", .. })));
    db.close().await;
    sqlx::query(&format!("drop database {database} with (force)")).execute(&admin).await.unwrap();
}

#[tokio::test]
async fn intel_postgres_delivery_mode_and_current_authority() {
    let Some((db, admin, database)) = test_database().await else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let op = actor("operator", Role::Operator);
    let writer = actor("writer", Role::Ingest);
    let mut intel = config();
    intel.sources.get_mut("acme.feed").unwrap().default_audience = Audience::PersonaEligible;
    intel.sources.get_mut("social.trends").unwrap().default_audience = Audience::PublicCleared;
    let hub = test_hub(db.clone(), HubConfig { intel, company_id: "acme".into(), ..Default::default() });
    let hidden = put(&hub, &op, batch("acme.feed", "Private report", Audience::OperatorOnly)).await.id;
    let mut visible = batch("acme.feed", "Public-facing report", Audience::PersonaEligible);
    visible.signals[0].external_id = "new-key".into();
    let collision = batch("acme.feed", "Public-facing report", Audience::PersonaEligible);
    assert!(matches!(get_signal(&hub, &writer, hidden).await, Err(DomainError::NotFound(_))));
    for (key, input) in [("visible", visible.clone()), ("hidden", collision)] {
        let ack = ingest_batch(&hub, &writer, input.clone(), Some(key)).await.unwrap();
        assert!(matches!(ack, SignalBatchOut::Accepted { accepted_count: 1, .. }));
        assert_eq!(ingest_batch(&hub, &writer, input.clone(), Some(key)).await.unwrap(), ack);
        let receipt = first(ingest_batch(&hub, &op, input, Some(key)).await.unwrap());
        if key == "visible" {
            assert!(receipt.created);
            assert!(get_signal(&hub, &writer, receipt.id).await.is_ok());
        } else {
            assert!(!receipt.created);
            assert_eq!(receipt.id, hidden);
            assert!(matches!(get_signal(&hub, &writer, receipt.id).await, Err(DomainError::NotFound(_))));
        }
    }

    let target = put(&hub, &op, batch("social.trends", "Cross-company target", Audience::PublicCleared)).await.id;
    assert!(matches!(get_signal(&hub, &writer, target).await, Err(DomainError::NotFound(_))));
    let target_signal = get_signal(&hub, &op, target).await.unwrap().signal;
    clear_signal(&hub, &op, target, clearance(&target_signal)).await.unwrap();
    let mut linked = visible;
    linked.signals[0].links.push(SignalLinkIn {
        target_type: LinkTargetType::Signal,
        target_id: target.to_string(),
        relation: LinkRelation::About,
    });
    let ack = ingest_batch(&hub, &writer, linked.clone(), Some("linked")).await.unwrap();
    assert_eq!(ingest_batch(&hub, &writer, linked.clone(), Some("linked")).await.unwrap(), ack);
    let mut removed_kind = hub.clone();
    Arc::make_mut(&mut removed_kind.config).intel.sources.get_mut("acme.feed").unwrap().kinds.clear();
    let mut revoked_target = hub.clone();
    Arc::make_mut(&mut revoked_target.config).intel.sources.get_mut("social.trends").unwrap().policy_version += 1;
    for input in [linked.clone(), {
        linked.run.run_id = "different-body".into();
        linked
    }] {
        assert_eq!(ingest_batch(&removed_kind, &writer, input.clone(), Some("linked")).await.unwrap_err().code(), "kind_not_allowed");
        assert_eq!(ingest_batch(&revoked_target, &writer, input, Some("linked")).await.unwrap_err().code(), "not_found");
    }
    db.close().await;
    sqlx::query(&format!("drop database {database} with (force)")).execute(&admin).await.unwrap();
}
