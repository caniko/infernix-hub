//! Binary Hub activity transfer v1. This is not the session JSONL export.
//! Quota history needs a history-only merge endpoint; observation ingestion
//! updates live provider state and does not deduplicate samples.

use std::{
    env, fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use contracts::{
    client::{ClientError, HubClient},
    providers::{ActivityAttemptIn, ActivityExchangeIn, ActivityIngestBatch, ActivityScope, AttemptDetail, CaptureState, RequestRow},
};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    digest::{SHA256, digest},
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const HEADER: &[u8] = b"FLEETIX-HUB-TRANSFER\x01";
const NONCE_SIZE: usize = 12;
const MAX_ARCHIVE: usize = 64 * 1024 * 1024;
const MAX_EXCHANGES: usize = 20_000;
const MAX_EVENT_BYTES: usize = 32 * 1024;
const SOURCE_PREFIX: &str = "hubctl-v1:";

// The header is authenticated as AAD; the final 16 bytes are the GCM tag.
fn key(secret: &[u8]) -> Result<LessSafeKey> {
    ensure!(!secret.is_empty(), "HUB_TRANSFER_KEY is empty");
    let hash = digest(&SHA256, secret);
    Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, hash.as_ref()).map_err(|_| anyhow::anyhow!("invalid AES key"))?))
}

fn seal(secret: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0; NONCE_SIZE];
    SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("could not generate transfer nonce"))?;
    let mut body = payload.to_vec();
    key(secret)?
        .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(HEADER), &mut body)
        .map_err(|_| anyhow::anyhow!("could not encrypt transfer"))?;
    ensure!(HEADER.len() + nonce.len() + body.len() <= MAX_ARCHIVE, "transfer archive exceeds size limit");
    Ok([HEADER, nonce.as_slice(), &body].concat())
}

fn open(secret: &[u8], mut envelope: Vec<u8>) -> Result<Vec<u8>> {
    let offset = HEADER.len() + NONCE_SIZE;
    ensure!(envelope.starts_with(HEADER) && envelope.len() >= offset + AES_256_GCM.tag_len(), "invalid or unsupported transfer envelope");
    let nonce = Nonce::try_assume_unique_for_key(&envelope[HEADER.len()..offset]).map_err(|_| anyhow::anyhow!("invalid transfer nonce"))?;
    let body = key(secret)?
        .open_in_place(nonce, Aad::from(HEADER), &mut envelope[offset..])
        .map_err(|_| anyhow::anyhow!("transfer authentication failed (wrong key or damaged archive)"))?;
    Ok(body.to_vec())
}

#[derive(Serialize, Deserialize)]
struct Archive {
    exchanges: Vec<ActivityExchangeIn>,
}

fn exchange_row(row: RequestRow, attempts: Vec<AttemptDetail>) -> ActivityExchangeIn {
    let import_id = row.source_exchange_id.strip_prefix(SOURCE_PREFIX).and_then(|s| Uuid::parse_str(s).ok()).unwrap_or(row.id);
    let source_exchange_id = format!("{SOURCE_PREFIX}{import_id}");
    ActivityExchangeIn {
        attempts: attempts
            .into_iter()
            .map(|a| ActivityAttemptIn {
                source_attempt_id: format!("{source_exchange_id}:{}", a.attempt_index),
                attempt_index: a.attempt_index,
                provider_id: a.provider_id,
                account_id: a.account_id,
                moved_from_account_id: a.moved_from_account_id,
                requested_model: a.requested_model,
                effective_model: a.effective_model,
                effort: a.effort,
                status_code: a.status_code,
                outcome: a.outcome,
                error_message: None,
                latency_ms: a.latency_ms,
                ttft_ms: a.ttft_ms,
                input_tokens: a.input_tokens,
                output_tokens: a.output_tokens,
                cache_read_tokens: a.cache_read_tokens,
                cache_write_tokens: a.cache_write_tokens,
                thinking_tokens: a.thinking_tokens,
                fast_mode: false,
                capture_state: CaptureState::MetadataOnly,
                captured_body: None,
                started_at: a.started_at,
                finished_at: a.finished_at,
            })
            .collect(),
        source_exchange_id,
        source: row.source,
        purpose: row.purpose,
        client: row.client,
        client_version: None,
        transport: row.transport,
        session_id: row.session_id,
        parent_session_id: None,
        parent_edge_kind: None,
        provider_id: row.provider_id,
        account_id: row.account_id,
        requested_model: row.requested_model,
        effective_model: row.effective_model,
        effort: row.effort,
        outcome: row.outcome,
        status_code: row.status_code,
        error_message: None,
        latency_ms: row.latency_ms,
        ttft_ms: row.ttft_ms,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cache_read_tokens: row.cache_read_tokens,
        cache_write_tokens: row.cache_write_tokens,
        thinking_tokens: row.thinking_tokens,
        fast_mode: false,
        capture_state: CaptureState::MetadataOnly,
        searchable_text: None,
        occurred_at: row.occurred_at,
    }
}

fn transfer_key() -> Result<std::ffi::OsString> {
    env::var_os("HUB_TRANSFER_KEY").filter(|s| !s.is_empty()).context("set HUB_TRANSFER_KEY to a shared high-entropy secret")
}

/// Publish with an atomic no-clobber link: never replace an existing destination,
/// and never make a partial archive visible under the requested name.
fn write_archive(path: &Path, encrypted: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(".hubctl-transfer-{}", Uuid::new_v4()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    let result = (|| {
        file.write_all(encrypted)?;
        file.sync_all()?;
        fs::hard_link(&tmp, path)
    })();
    drop(file);
    let _ = fs::remove_file(tmp);
    result.context("publishing transfer archive (destination must not exist)")
}

pub(super) async fn export(client: &HubClient, path: &Path) -> Result<()> {
    let key = transfer_key()?;
    let mut archive = Archive { exchanges: Vec::new() };
    let mut size = 0;
    let mut scope = ActivityScope { from: Some(DateTime::<Utc>::UNIX_EPOCH), to: Some(Utc::now()), limit: Some(500), ..Default::default() };
    loop {
        let page = client.activity_requests(&scope).await?;
        for row in page.items {
            let detail = client.activity_request_detail(row.id).await?;
            let event = exchange_row(detail.exchange, detail.attempts);
            let bytes = serde_json::to_vec(&event)?.len();
            ensure!(bytes <= MAX_EVENT_BYTES, "activity event exceeds the transfer record limit");
            size += bytes + 1;
            ensure!(size <= MAX_ARCHIVE && archive.exchanges.len() < MAX_EXCHANGES, "activity exceeds offline transfer limits");
            archive.exchanges.push(event);
        }
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        scope.cursor = Some(next_cursor);
    }
    let encrypted = seal(key.as_os_str().as_bytes(), &serde_json::to_vec(&archive)?)?;
    write_archive(path, &encrypted)?;
    println!("Exported {} activity events (metadata only; quota history not included).", archive.exchanges.len());
    Ok(())
}

pub(super) async fn import(client: &HubClient, path: &Path) -> Result<()> {
    let key = transfer_key()?;
    ensure!(fs::metadata(path)?.is_file(), "transfer input is not a regular file");
    let mut encrypted = Vec::new();
    fs::File::open(path)?.take((MAX_ARCHIVE + 1) as u64).read_to_end(&mut encrypted)?;
    ensure!(encrypted.len() <= MAX_ARCHIVE, "transfer archive exceeds size limit");
    let data = open(key.as_os_str().as_bytes(), encrypted)?;
    let mut archive: Archive = serde_json::from_slice(&data).map_err(|_| anyhow::anyhow!("invalid authenticated transfer payload"))?;
    ensure!(archive.exchanges.len() <= MAX_EXCHANGES, "too many activity events in transfer archive");
    for event in &mut archive.exchanges {
        ensure!(
            event.source_exchange_id.strip_prefix(SOURCE_PREFIX).and_then(|s| Uuid::parse_str(s).ok()).is_some(),
            "invalid activity identity in transfer archive"
        );
        // A valid key authenticates the sender, not the content. Never import
        // free-form text, errors, captures, or configuration via this workflow.
        event.searchable_text = None;
        event.error_message = None;
        event.capture_state = CaptureState::MetadataOnly;
        for attempt in &mut event.attempts {
            attempt.capture_state = CaptureState::MetadataOnly;
            attempt.captured_body = None;
            attempt.error_message = None;
        }
        ensure!(serde_json::to_vec(event)?.len() <= MAX_EVENT_BYTES, "activity event exceeds the transfer record limit");
    }
    let mut accepted = 0;
    let mut skipped = 0;
    for events in archive.exchanges.chunks(32) {
        let mut exchanges = Vec::new();
        for event in events {
            let id = event.source_exchange_id[SOURCE_PREFIX.len()..].parse()?;
            match client.activity_request_detail(id).await {
                Ok(_) => skipped += 1, // Importing back into the source Hub.
                Err(ClientError::Api { status: 404, .. }) => exchanges.push(event.clone()),
                Err(err) => return Err(err.into()),
            }
        }
        if !exchanges.is_empty() {
            let ack = client
                .ingest_activity(&ActivityIngestBatch { source_host: "hubctl-transfer".into(), source_user: "v1".into(), exchanges })
                .await?;
            accepted += ack.accepted;
            skipped += ack.deduplicated;
        }
    }
    println!("Imported {accepted} activity events; skipped {skipped} already present.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_envelope_round_trip() {
        let payload = br#"{"exchanges":[]}"#;
        let encrypted = seal(b"test key", payload).unwrap();
        assert_ne!(encrypted, seal(b"test key", payload).unwrap());
        assert_eq!(open(b"test key", encrypted).unwrap(), payload);
    }

    #[test]
    fn transfer_envelope_rejects_wrong_key() {
        let ciphertext = seal(b"test key", b"retained activity").unwrap();
        assert!(open(b"other key", ciphertext).is_err());
    }

    #[test]
    fn transfer_envelope_rejects_tampering() {
        let encrypted = seal(b"test key", b"retained activity").unwrap();
        for i in [HEADER.len() - 1, HEADER.len(), HEADER.len() + NONCE_SIZE, encrypted.len() - 1] {
            let mut changed = encrypted.clone();
            changed[i] ^= 1;
            assert!(open(b"test key", changed).is_err());
        }
    }
}
