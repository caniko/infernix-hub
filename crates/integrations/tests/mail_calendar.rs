use hub_integrations::{calendar_sync, imap_sync, local_llm_bi, mail_parse};
use sha2::{Digest, Sha256};
use std::io::Cursor;

const MESSAGE: &[u8] = b"From: Sender <sender@example.org>\r\nTo: you@example.org\r\nMessage-ID: <child@example.org>\r\nReferences: <root@example.org> <parent@example.org>\r\nSubject: =?UTF-8?B?Q2Fmw6k=?=\r\nX-Spam-Flag: NO\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>world</b></p>";

#[test]
fn mime_headers_html_and_original_hash() {
    let msg = mail_parse::parse_eml(MESSAGE, "INBOX").unwrap();
    assert_eq!(msg.subject, "Café");
    assert_eq!(msg.from_name, "Sender");
    assert_eq!(msg.from_domain, "example.org");
    assert_eq!(msg.thread_key, "root@example.org");
    assert_eq!(msg.raw_headers["x-spam-flag"], "NO");
    assert_eq!(msg.body_text.trim(), "Hello world");
    assert_eq!(msg.content_sha256.unwrap(), hex::encode(Sha256::digest(MESSAGE)));
}

#[test]
fn multipart_prefers_plain_text_and_reports_attachment_names() {
    let message = b"Subject: Re: Status\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=mix\r\n\r\n\
--mix\r\nContent-Type: multipart/alternative; boundary=alt\r\n\r\n\
--alt\r\nContent-Type: text/html\r\n\r\n<p>HTML</p>\r\n\
--alt\r\nContent-Type: text/plain\r\n\r\nPreferred plain text\r\n--alt--\r\n\
--mix\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=report.txt\r\n\r\nAttachment\r\n--mix--\r\n";
    let message = mail_parse::parse_eml(message, "INBOX").unwrap();
    assert_eq!(message.body_text.trim(), "Preferred plain text");
    assert_eq!(message.thread_key, "status");
    assert!(message.has_attachments);
    assert_eq!(message.attachment_names, ["report.txt"]);
}

#[test]
fn mbox_splits_takeout_envelopes_and_unquotes_body() {
    let data = b"From sender@example.org Sat Jan  1 00:00:00 2022\nSubject: First\n\n>From body\nFrom not a separator\nFrom sender@example.org Sat Jan  1 00:00:01 2022\nSubject: Second\n\nSecond body\n";
    let messages = mail_parse::iter_mbox_messages(Cursor::new(data), "Archive").collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].subject, "First");
    assert!(messages[0].body_text.contains("From body\nFrom not a separator"));
    assert_eq!(messages[1].folder, "Archive");
}

#[test]
fn ical_unfolds_and_keeps_recurrence_and_timezone() {
    let events = calendar_sync::parse_ics("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:one\r\nSUMMARY:Folded \r\n line\r\nDTSTART;VALUE=DATE:20260701\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:two\r\nDTSTART;TZID=Europe/Berlin:20260701T120000\r\nDURATION:PT1H\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n").unwrap();
    assert_eq!(events[0].title, "Folded line");
    assert!(events[0].all_day);
    assert_eq!(events[0].rrule.as_deref(), Some("FREQ=WEEKLY"));
    assert_eq!(events[1].timezone, "Europe/Berlin");
    assert_eq!(events[1].starts_at.to_rfc3339(), "2026-07-01T10:00:00+00:00");
    assert_eq!((events[1].ends_at - events[1].starts_at).num_hours(), 1);
}

#[test]
fn plain_imap_is_loopback_only() {
    for host in ["example.org", "127.0.0.2", "localhost.example.org", "0.0.0.0"] {
        assert!(imap_sync::validate_transport(host, imap_sync::TlsMode::Plain).is_err());
    }
    assert!(imap_sync::validate_transport("::1", imap_sync::TlsMode::Plain).is_ok());
    assert!(imap_sync::validate_transport("example.org", imap_sync::TlsMode::Tls).is_ok());
}

#[test]
fn llm_json_is_strict_but_accepts_fences() {
    let result = local_llm_bi::parse_response("```json\n{\"classification\":\"invoice\",\"is_junk\":false,\"priority\":\"high\",\"summary\":\"Bill due\",\"action_items\":[\"Pay bill\"],\"topics\":[],\"vendor\":\"Acme\"}\n```").unwrap();
    assert_eq!(result.vendor.as_deref(), Some("Acme"));
    assert!(local_llm_bi::parse_response("{\"classification\":\"invoice\"}").is_err());
}

#[tokio::test]
async fn imap_resets_uidvalidity_and_fetches_without_modifying_flags() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(stream);
        socket.write_all(b"* OK ready\r\n").await.unwrap();
        for (expected, response) in [
            ("H1 LOGIN \"user\" \"test\"\r\n", "H1 OK\r\n".to_string()),
            ("H2 SELECT \"INBOX\"\r\n", "* OK [UIDVALIDITY 7] current\r\nH2 OK\r\n".into()),
            ("H3 UID SEARCH UID 1:*\r\n", "* SEARCH 12\r\nH3 OK\r\n".into()),
            (
                "H4 UID FETCH 12 (UID FLAGS BODY.PEEK[])\r\n",
                format!(
                    "* 1 FETCH (BODY[] {{{}}}\r\n{} UID 12 FLAGS (\\Seen))\r\nH4 OK\r\n",
                    MESSAGE.len(),
                    std::str::from_utf8(MESSAGE).unwrap()
                ),
            ),
        ] {
            let mut command = String::new();
            socket.read_line(&mut command).await.unwrap();
            assert_eq!(command, expected);
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let connection = imap_sync::Connection { host: "127.0.0.1", port, tls: imap_sync::TlsMode::Plain, username: "user", password: "test" };
    let cursor = serde_json::json!({"INBOX":{"uidvalidity":6,"last_uid":200}});
    let (batch, cursor) = imap_sync::sync_imap(&connection, &["INBOX".into()], &cursor, |_, raw| {
        assert_eq!(raw, MESSAGE);
        Ok(None)
    })
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(batch.len(), 1);
    assert!(batch[0].is_read);
    assert_eq!(batch[0].imap_location.as_ref().unwrap().uid, 12);
    assert_eq!(batch[0].imap_location.as_ref().unwrap().uidvalidity, 7);
    assert_eq!(cursor, serde_json::json!({"INBOX":{"uidvalidity":7,"last_uid":12}}));
}

#[tokio::test]
async fn ics_304_is_not_an_empty_snapshot() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/private?token=secret", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(stream);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            let n = socket.read_line(&mut line).await.unwrap();
            if n == 0 || line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }
        assert!(headers.to_ascii_lowercase().contains("if-none-match: \"v1\""));
        socket.write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let fetched = calendar_sync::fetch_ics_source(&reqwest::Client::new(), &url, None, Some("\"v1\"")).await.unwrap();
    assert!(matches!(fetched, calendar_sync::Snapshot::Unchanged));
    server.await.unwrap();
}
