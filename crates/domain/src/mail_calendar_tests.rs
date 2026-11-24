use std::str::FromStr;

use chrono::{TimeDelta, Utc};
use contracts::*;
use serde_json::Value;
use sqlx::postgres::PgConnectOptions;
use uuid::Uuid;

use super::*;
use crate::ports::*;

struct Unused;

macro_rules! unused_port {
    ($port:ident { $( $name:ident($($arg:ident: $ty:ty),*) -> $out:ty; )* }) => {
        #[async_trait::async_trait]
        impl $port for Unused {
            $(async fn $name(&self, $($arg: $ty),*) -> $out {
                panic!("mail/calendar must not call {}", stringify!($name))
            })*
        }
    };
}

unused_port!(Paperclip {
    create_issue(_req: &IssueRequest) -> PortResult<ForeignRef>;
    create_approval(_req: &ApprovalRequest) -> PortResult<ForeignRef>;
    get_approval(_id: &str) -> PortResult<ApprovalView>;
});
unused_port!(Windmill {
    run_generation(_job: &GenerationJob) -> PortResult<String>;
    schedule_publication(_timer: &PublicationTimer) -> PortResult<String>;
    job(_id: &str) -> PortResult<JobView>;
    cancel_job(_id: &str) -> PortResult<()>;
});
unused_port!(Comfy {
    upload(_url: &str, _name: &str, _mime: &str, _bytes: &[u8]) -> PortResult<()>;
    load(_url: &str) -> PortResult<EngineLoad>;
    subscribe(_url: &str, _client: &str, _tx: tokio::sync::mpsc::Sender<ProgressEvent>) -> PortResult<()>;
    outcome(_url: &str, _id: &str) -> PortResult<PromptOutcome>;
    fetch(_url: &str, _file: &OutputFile) -> PortResult<Vec<u8>>;
    cancel(_url: &str, _id: &str) -> PortResult<CancelOutcome>;
});
unused_port!(Publisher {
    submit(_req: &PublishRequest) -> PortResult<ProviderPost>;
    lookup(_account: &str, _key: &str) -> PortResult<Option<ProviderPost>>;
    reply(_req: &ReplyRequest) -> PortResult<ProviderPost>;
    lookup_reply(_account: &str, _key: &str) -> PortResult<Option<ProviderPost>>;
});
unused_port!(AssetStore {
    put(_sha: &str, _bytes: &[u8]) -> std::io::Result<String>;
    get(_key: &str) -> std::io::Result<Vec<u8>>;
});
unused_port!(Assistant {
    reply(_context: &Value, _turns: &[AssistantTurn]) -> PortResult<AssistantReply>;
});
unused_port!(GpuPeer {
    busy(_url: &str) -> PortResult<bool>;
});

fn user(name: &str) -> Actor {
    Actor { principal: Principal { id: Uuid::new_v4(), name: name.into(), role: Role::Operator }, correlation_id: None }
}

// Same opt-in convention as the repository's PostgreSQL journey tests.
#[tokio::test]
async fn mail_calendar_postgres() {
    let Ok(url) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let opts = PgConnectOptions::from_str(&url).unwrap();
    let admin = PgPool::connect_with(opts.clone()).await.unwrap();
    let database = format!("hub_mail_calendar_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create database {database}")).execute(&admin).await.unwrap();
    let db = PgPool::connect_with(opts.database(&database)).await.unwrap();
    MIGRATOR.run(&db).await.unwrap();
    let unused = Arc::new(Unused);
    let hub = Hub {
        db: db.clone(),
        config: Arc::new(HubConfig {
            principal_owners: BTreeMap::from([("alice-login".into(), "alice".into()), ("bob-login".into(), "bob".into())]),
            ..Default::default()
        }),
        paperclip: unused.clone(),
        windmill: unused.clone(),
        comfy: unused.clone(),
        publisher: unused.clone(),
        assets: unused.clone(),
        assistant: unused.clone(),
        gpu_peer: unused,
        channels: Default::default(),
        pipelines: Default::default(),
        topics: Default::default(),
        providers: Default::default(),
        notified: Default::default(),
        expected_offline_hosts: Default::default(),
        feeds: Default::default(),
    };
    let system = Actor::system();
    let (alice, bob) = (user("alice-login"), user("bob-login"));
    let account = hub
        .upsert_mail_account_system(
            &system,
            "alice",
            &serde_json::from_str(r#"{"label":"test","provider":"imap","transport":"imap"}"#).unwrap(),
        )
        .await
        .unwrap();
    let now = chrono::DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap();
    let invoice = MailMessageIn {
        subject: "Invoice INV-104".into(),
        from_addr: "billing@example.com".into(),
        body_text: "EUR 49.99. Please pay by 2026-12-03.".into(),
        imap_location: Some(MailImapLocationIn { folder: "INBOX".into(), uidvalidity: 1, uid: 1 }),
        received_at: Some(now),
        ..Default::default()
    };
    let request = MailIngestRequest {
        account_id: account.id,
        messages: vec![invoice.clone(), MailMessageIn { subject: "Receipt order ORD-203".into(), imap_location: None, ..invoice }],
    };
    assert!(hub.list_mail_accounts(&bob).await.unwrap().is_empty());
    assert!(matches!(hub.ingest_mail_messages(&bob, &request).await, Err(DomainError::NotFound(_))));
    let mail_lease = hub.acquire_mail_sync_lease(&system, "alice", account.id, 60).await.unwrap().unwrap();
    assert!(hub.acquire_mail_sync_lease(&system, "alice", account.id, 60).await.unwrap().is_none());
    assert_eq!(hub.ingest_mail_messages_system(&system, "alice", mail_lease, &request).await.unwrap().inserted, 2);
    assert_eq!(hub.ingest_mail_messages(&alice, &request).await.unwrap().duplicates, 2);
    hub.update_mail_account_sync_state(&system, "alice", account.id, mail_lease, "ok", None, None).await.unwrap();
    hub.release_mail_sync_lease(&system, "alice", account.id, mail_lease).await.unwrap();
    let search = MailSearchQuery { q: Some("INV-104".into()), ..Default::default() };
    let messages = hub.list_mail_messages(&alice, &search).await.unwrap();
    assert_eq!(messages.len(), 1);
    let id = messages[0].id;
    assert!(matches!(hub.get_mail_message(&bob, id).await, Err(DomainError::NotFound(_))));
    assert!(matches!(hub.override_mail_triage(&bob, id, &MailTriageOverride::default()).await, Err(DomainError::NotFound(_))));
    assert!(matches!(
        hub.list_mail_messages(&bob, &MailSearchQuery { account_id: Some(account.id), ..Default::default() }).await,
        Err(DomainError::NotFound(_))
    ));
    assert_eq!(hub.get_mail_message(&alice, id).await.unwrap().analysis.entities.amounts[0].amount, "49.99");
    let overview = hub.mail_bi_overview(&alice).await.unwrap();
    assert_eq!(overview.recurring_vendors[0].message_count, 2);
    assert_eq!(overview.open_action_items.len(), 2);
    assert_eq!((overview.unread_signal_count, overview.recurring_vendor_count, overview.open_action_item_count), (2, 1, 2));
    assert_eq!(hub.mail_bi_overview(&bob).await.unwrap().total_count, 0);
    hub.override_mail_triage(&alice, id, &MailTriageOverride { classification: None, is_junk: Some(Some(true)) }).await.unwrap();
    assert!(hub.list_mail_messages(&alice, &search).await.unwrap().is_empty());
    let overview = hub.mail_bi_overview(&alice).await.unwrap();
    assert_eq!(
        (overview.unread_count, overview.unread_signal_count, overview.recurring_vendor_count, overview.open_action_item_count),
        (2, 1, 0, 1)
    );
    assert_eq!(hub.list_unenriched_mail(&system, "alice", 100, true).await.unwrap().len(), 1);
    assert_eq!(hub.list_unenriched_mail(&system, "alice", 100, false).await.unwrap().len(), 2);
    let analysis = hub.get_mail_message(&alice, id).await.unwrap().analysis;
    assert!(matches!(hub.save_llm_analysis(&system, "bob", id, "local-test", &analysis).await, Err(DomainError::NotFound(_))));
    hub.save_llm_analysis(&system, "alice", id, "local-test", &analysis).await.unwrap();
    assert_eq!(hub.get_mail_message(&alice, id).await.unwrap().analysis.user_override_junk, Some(true));
    hub.override_mail_triage(&alice, id, &MailTriageOverride { classification: None, is_junk: Some(None) }).await.unwrap();
    assert_eq!(hub.list_mail_messages(&alice, &search).await.unwrap().len(), 1);
    let digest = hub.create_mail_bi_digest(&alice, "day", now - TimeDelta::days(1), now + TimeDelta::days(1)).await.unwrap();
    assert_eq!(digest.metrics_json["total_count"], 2);
    assert_eq!(hub.create_mail_bi_digest(&alice, "day", now - TimeDelta::days(1), now + TimeDelta::days(1)).await.unwrap().id, digest.id);
    assert!(hub.list_mail_bi_digests(&bob).await.unwrap().is_empty());

    let mut source: CalendarSourceUpsert = serde_json::from_str(r#"{"label":"test","provider":"google_ics"}"#).unwrap();
    let calendar = hub.upsert_calendar_source_system(&system, "alice", &source).await.unwrap();
    assert!(matches!(hub.acquire_calendar_sync_lease(&system, "bob", calendar.id, 60).await, Err(DomainError::NotFound(_))));
    let lease = hub.acquire_calendar_sync_lease(&system, "alice", calendar.id, 60).await.unwrap().unwrap();
    let to = now + TimeDelta::days(4);
    let event: CalendarEventIn = serde_json::from_value(serde_json::json!({
        "uid":"test", "starts_at":now, "ends_at":now+TimeDelta::hours(1), "rrule":"FREQ=DAILY;COUNT=2",
    }))
    .unwrap();
    hub.replace_calendar_source_events(&system, "alice", calendar.id, lease, Some("etag"), std::slice::from_ref(&event)).await.unwrap();
    assert!(hub.list_calendar_sources(&bob).await.unwrap().is_empty());
    assert!(hub.list_calendar_events(&bob, now, to).await.unwrap().is_empty());
    source.visibility = "shared".into();
    hub.upsert_calendar_source_system(&system, "alice", &source).await.unwrap();
    assert_eq!(hub.list_calendar_events(&bob, now, to).await.unwrap().len(), 2);
    let lease = hub.acquire_calendar_sync_lease(&system, "alice", calendar.id, 60).await.unwrap().unwrap();
    hub.record_calendar_sync_error(&system, "alice", calendar.id, lease, "fetch incomplete").await.unwrap();
    let sources = hub.list_calendar_sources(&alice).await.unwrap();
    assert_eq!(sources[0].sync_etag.as_deref(), Some("etag"));
    assert_eq!(hub.calendar_agenda(&alice, now, to).await.unwrap().events.len(), 2);
    let lease = hub.acquire_calendar_sync_lease(&system, "alice", calendar.id, 60).await.unwrap().unwrap();
    let skipped = now + TimeDelta::days(1);
    let cancellation = CalendarEventIn {
        recurrence_id: skipped.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        starts_at: skipped,
        ends_at: skipped,
        status: "cancelled".into(),
        rrule: None,
        ..event.clone()
    };
    assert!(matches!(
        hub.replace_calendar_source_events(&system, "bob", calendar.id, lease, None, &[]).await,
        Err(DomainError::NotFound(_))
    ));
    hub.replace_calendar_source_events(&system, "alice", calendar.id, lease, None, &[event, cancellation]).await.unwrap();
    assert_eq!(hub.calendar_agenda(&alice, now, to).await.unwrap().events.len(), 1);
    let lease = hub.acquire_calendar_sync_lease(&system, "alice", calendar.id, 60).await.unwrap().unwrap();
    hub.replace_calendar_source_events(&system, "alice", calendar.id, lease, None, &[]).await.unwrap();
    assert!(hub.calendar_agenda(&alice, now, to).await.unwrap().events.is_empty());

    db.close().await;
    sqlx::query(&format!("drop database {database}")).execute(&admin).await.unwrap();
}
