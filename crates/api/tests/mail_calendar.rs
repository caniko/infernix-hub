use contracts::{
    MailSearchQuery, Role,
    client::{ClientError, HubClient},
};
use domain::sqlx;
use fleetix_hubd::bootstrap::{self, Settings};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn imports_are_idempotent_and_mail_isolated_by_principal() {
    let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
        eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
        return;
    };
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    let name = format!("t_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
    let mut url = reqwest::Url::parse(&base).unwrap();
    url.set_path(&format!("/{name}"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut settings = Settings::fixture(url.as_str(), &listener.local_addr().unwrap().to_string());
    settings.asset_dir = std::env::temp_dir().join(&name);
    settings.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
    settings.seed_file = Some(settings.registry_dir.join("hub.fixture.json"));
    tokio::fs::create_dir_all(&settings.asset_dir).await.unwrap();
    let owners = settings.asset_dir.join("owners.json");
    tokio::fs::write(&owners, r#"{"can":"can-mail","dejana":"dejana-mail"}"#).await.unwrap();
    settings.principal_owners_config = Some(owners);
    let calendar_config = settings.asset_dir.join("calendar.json");
    let credential = settings.asset_dir.join("calendar-url");
    let ics = settings.asset_dir.join("calendar.ics");
    tokio::fs::write(
        &calendar_config,
        json!({"sources": [
            {"owner":"can-mail","provider":"ics_url","label":"private","url_file":credential}
        ]})
        .to_string(),
    )
    .await
    .unwrap();
    settings.calendar_config = Some(calendar_config);
    let (state, _) = bootstrap::build(&settings).await.unwrap();
    let mut clients = Vec::new();
    for principal in ["can", "dejana"] {
        let token = Uuid::new_v4().to_string();
        domain::system::issue_token(&state.hub.db, principal, Role::Member, Some(token.clone())).await.unwrap();
        clients.push(HubClient::new(&settings.public_url, token));
    }
    let server = tokio::spawn(async move { axum::serve(listener, fleetix_hubd::router(state)).await.unwrap() });
    let [can, dejana] = &clients[..] else { unreachable!() };
    assert!(dejana.list_calendar_sources().await.unwrap().is_empty());
    assert_eq!(can.sync_calendar().await.unwrap()[0].status, "awaiting_credentials");
    let event_day = (chrono::Utc::now() + chrono::Duration::days(2)).format("%Y%m%d");
    tokio::fs::write(
        &ics,
        format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:one\r\nDTSTART;VALUE=DATE:{event_day}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"),
    )
    .await
    .unwrap();
    tokio::fs::write(&credential, reqwest::Url::from_file_path(&ics).unwrap().as_str()).await.unwrap();
    let synced = can.sync_calendar().await.unwrap();
    assert_eq!((synced[0].status.as_str(), synced[0].event_count), ("ok", 1));
    assert_eq!(can.calendar_agenda_days(14).await.unwrap().events.len(), 1);
    assert!(dejana.calendar_agenda_days(14).await.unwrap().events.is_empty());
    let common =
        integrations::mail_parse::parse_eml(b"From: billing@example.org\r\nSubject: Invoice common\r\n\r\nInvoice INV-123", "INBOX")
            .unwrap();
    for (client, tag) in [(can, "private-can"), (dejana, "private-dejana")] {
        let unique =
            integrations::mail_parse::parse_eml(format!("From: work@example.org\r\nSubject: {tag}\r\n\r\nBody").as_bytes(), "INBOX")
                .unwrap();
        let batch = json!({"account":"same-label","provider":"export_dir","messages":[common,unique]});
        let first: contracts::MailIngestResult = client.post("/mail/ingest", &batch).await.unwrap();
        let repeat: contracts::MailIngestResult = client.post("/mail/ingest", &batch).await.unwrap();
        assert_eq!((first.inserted, first.duplicates, repeat.inserted, repeat.duplicates), (2, 0, 0, 2));
        let account = client.list_mail_accounts().await.unwrap();
        assert_eq!(account.len(), 1);
        assert_eq!(account[0].owner, tag.strip_prefix("private-").unwrap().to_string() + "-mail");
        assert_eq!(account[0].message_count, 2);
        assert_eq!(client.mail_bi_overview().await.unwrap().total_count, 2);
    }
    let search = |s: &str| MailSearchQuery { q: Some(s.into()), ..Default::default() };
    let can_messages = can.list_mail_messages(&search("private")).await.unwrap();
    let dejana_messages = dejana.list_mail_messages(&search("private")).await.unwrap();
    assert_eq!(can_messages.len(), 1);
    assert_eq!(dejana_messages.len(), 1);
    assert_eq!(can_messages[0].subject, "private-can");
    assert_eq!(dejana_messages[0].subject, "private-dejana");
    assert!(matches!(can.get_mail_message(dejana_messages[0].id).await, Err(ClientError::Api { status: 404, .. })));

    server.abort();
    let _ = server.await;
    sqlx::query(&format!("drop database {name} with (force)")).execute(&admin).await.unwrap();
    tokio::fs::remove_dir_all(&settings.asset_dir).await.unwrap();
}
