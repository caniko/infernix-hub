//! Owner-scoped mail and calendar routes, and declarative sync workers.
//!
//! The owner config is a JSON object mapping principal names to owners. The
//! mail config uses an `accounts` array of `{owner,label,provider,transport,...}`
//! and an optional `bi` object. The calendar config uses a `sources` array of
//! `{owner,label,provider,...}`. Secret settings use `<name>_credential` for a
//! systemd credential ID and `<name>_file` for an optional filesystem fallback.
//! Only administrators can supply these configs; HTTP callers never supply
//! server-side destinations or credential paths.

use crate::{ApiErr, AppState, Caller, R};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use contracts::*;
use domain::{Actor, DResult, DomainError, Hub, mail::MailSyncState};
use integrations::{
    calendar_sync::{self, Auth, Snapshot},
    imap_sync::{self, Connection, TlsMode},
    local_llm_bi, mail_parse,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

const LEASE_SECONDS: u32 = 600;
const PASS_TIMEOUT: Duration = Duration::from_secs(480);

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MailRuntimeConfig {
    sync_interval_seconds: u64,
    archive_raw_dir: Option<PathBuf>,
    accounts: Vec<AccountConfig>,
    bi: BiConfig,
}

impl Default for MailRuntimeConfig {
    fn default() -> Self {
        Self { sync_interval_seconds: 300, archive_raw_dir: None, accounts: Vec::new(), bi: BiConfig::default() }
    }
}

#[derive(Deserialize)]
struct AccountConfig {
    owner: String,
    #[serde(flatten)]
    account: MailAccountUpsert,
    host: Option<String>,
    port: Option<u16>,
    tls_mode: Option<String>,
    folders: Option<Vec<String>>,
    export_dir: Option<String>,
    username: Option<String>,
    username_credential: Option<String>,
    username_file: Option<PathBuf>,
    password_credential: Option<String>,
    password_file: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BiConfig {
    enable: bool,
    interval_seconds: u64,
    max_batch_per_tick: u32,
    skip_junk: bool,
    llm: LlmConfig,
}

impl Default for BiConfig {
    fn default() -> Self {
        Self { enable: false, interval_seconds: 120, max_batch_per_tick: 10, skip_junk: true, llm: LlmConfig::default() }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LlmConfig {
    enable: bool,
    base_url: String,
    model: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self { enable: false, base_url: "http://127.0.0.1:8013/v1".into(), model: String::new() }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CalendarRuntimeConfig {
    sync_interval_seconds: u64,
    sources: Vec<SourceConfig>,
}

impl Default for CalendarRuntimeConfig {
    fn default() -> Self {
        Self { sync_interval_seconds: 300, sources: Vec::new() }
    }
}

#[derive(Deserialize)]
struct SourceConfig {
    owner: String,
    #[serde(flatten)]
    source: CalendarSourceUpsert,
    url: Option<String>,
    url_credential: Option<String>,
    url_file: Option<PathBuf>,
    username: Option<String>,
    username_credential: Option<String>,
    username_file: Option<PathBuf>,
    password_credential: Option<String>,
    password_file: Option<PathBuf>,
    bearer_token_credential: Option<String>,
    bearer_token_file: Option<PathBuf>,
    service_account_credential: Option<String>,
    service_account_file: Option<PathBuf>,
    calendar_id: Option<String>,
}

pub struct Runtime {
    mail: MailRuntimeConfig,
    calendar: CalendarRuntimeConfig,
    client: reqwest::Client,
    credentials_dir: Option<PathBuf>,
}

impl Runtime {
    pub fn new(mut mail: MailRuntimeConfig, calendar: CalendarRuntimeConfig) -> anyhow::Result<Self> {
        for config in &mut mail.accounts {
            let transport = &mut config.account.config_json;
            transport.host = config.host.take().or(transport.host.take());
            transport.port = config.port.or(transport.port);
            transport.tls_mode = config.tls_mode.take().or(transport.tls_mode.take());
            transport.folders = config.folders.take().unwrap_or_else(|| std::mem::take(&mut transport.folders));
            transport.export_dir = config.export_dir.take().or(transport.export_dir.take());
        }
        let mut accounts = BTreeSet::new();
        let mut sources = BTreeSet::new();
        anyhow::ensure!(
            mail.accounts.iter().all(|c| accounts.insert((&c.owner, &c.account.label))),
            "duplicate mail account configuration"
        );
        anyhow::ensure!(
            calendar.sources.iter().all(|c| sources.insert((&c.owner, &c.source.label))),
            "duplicate calendar source configuration"
        );
        anyhow::ensure!(
            !mail.bi.enable || !mail.bi.llm.enable || !mail.bi.llm.model.trim().is_empty(),
            "a local BI model is required when enabled"
        );
        let client =
            reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(45)).build()?;
        Ok(Self { mail, calendar, client, credentials_dir: std::env::var_os("CREDENTIALS_DIRECTORY").map(Into::into) })
    }

    pub async fn register(&self, hub: &Hub, mail_config_managed: bool) -> DResult<()> {
        let actor = Actor::system();
        if mail_config_managed {
            hub.register_configured_mail_accounts(&actor, self.mail.accounts.iter().map(|c| (c.owner.as_str(), &c.account))).await?;
        }
        for config in &self.calendar.sources {
            hub.upsert_calendar_source_system(&actor, &config.owner, &config.source).await?;
        }
        Ok(())
    }

    pub fn spawn(self: &Arc<Self>, hub: Hub) {
        if !self.mail.accounts.is_empty() {
            let (runtime, hub) = (self.clone(), hub.clone());
            tokio::spawn(async move {
                let mut tick = interval(runtime.mail.sync_interval_seconds);
                loop {
                    tick.tick().await;
                    runtime.sync_mail(&hub, None).await;
                }
            });
        }
        if !self.calendar.sources.is_empty() {
            let (runtime, hub) = (self.clone(), hub.clone());
            tokio::spawn(async move {
                let mut tick = interval(runtime.calendar.sync_interval_seconds);
                loop {
                    tick.tick().await;
                    runtime.sync_calendar(&hub, None).await;
                }
            });
        }
        if self.llm().is_some() {
            let runtime = self.clone();
            tokio::spawn(async move {
                let mut tick = interval(runtime.mail.bi.interval_seconds);
                loop {
                    tick.tick().await;
                    if let Err(error) = runtime.enrich_mail(&hub).await {
                        tracing::warn!(code = error.code(), "mail BI pass failed");
                    }
                }
            });
        }
    }

    fn llm(&self) -> Option<&LlmConfig> {
        (self.mail.bi.enable && self.mail.bi.llm.enable).then_some(&self.mail.bi.llm)
    }

    async fn enrich_mail(&self, hub: &Hub) -> DResult<()> {
        let Some(llm) = self.llm() else {
            return Ok(());
        };
        let actor = Actor::system();
        let mut remaining = self.mail.bi.max_batch_per_tick.min(100);
        // Round-robin owners within each pass so imports by one owner don't fill the batch.
        let mut owners = hub.mail_owners_system(&actor).await?;
        if !owners.is_empty() {
            let first = Utc::now().timestamp().unsigned_abs() / self.mail.bi.interval_seconds.max(1);
            let count = owners.len();
            owners.rotate_left(first as usize % count);
        }
        for owner in owners {
            if remaining == 0 {
                break;
            }
            for message in hub.list_unenriched_mail(&actor, &owner, remaining, self.mail.bi.skip_junk).await? {
                let snippet: String = message.body_text.chars().take(4000).collect();
                let input = json!({"subject": message.message.subject, "from": message.message.from_addr, "snippet": snippet});
                let Ok(model) = local_llm_bi::analyze(&self.client, &llm.base_url, &llm.model, &input.to_string()).await else {
                    // The next interval is the retry, not every message in this batch.
                    return Ok(());
                };
                let mut analysis = message.analysis;
                analysis.classification = model.classification;
                analysis.is_junk = model.is_junk;
                analysis.junk_score = if model.is_junk { 1.0 } else { 0.0 };
                analysis.priority = model.priority;
                analysis.summary = model.summary;
                analysis.action_items = model.action_items;
                analysis.topics = model.topics;
                analysis.entities.vendor = model.vendor.or(analysis.entities.vendor);
                analysis.analyzed_at = Utc::now();
                hub.save_llm_analysis(&actor, &owner, message.message.id, &llm.model, &analysis).await?;
                remaining -= 1;
            }
        }
        Ok(())
    }

    async fn credential(&self, name: Option<&str>, fallback: Option<&FsPath>) -> io::Result<Option<String>> {
        read_runtime_credential(self.credentials_dir.as_deref(), name, fallback).await
    }

    async fn sync_mail(&self, hub: &Hub, caller: Option<(&str, bool)>) {
        let actor = Actor::system();
        for config in self.mail.accounts.iter().filter(|c| {
            c.account.enabled
                && matches!(c.account.transport.as_str(), "imap" | "export_dir")
                && caller.is_none_or(|(owner, human)| human || owner == c.owner)
        }) {
            if let Err(error) = self.sync_account(hub, &actor, config, caller.map(|(owner, _)| owner)).await {
                tracing::warn!(code = error.code(), "mail sync state could not be updated");
            }
        }
    }

    async fn sync_account(&self, hub: &Hub, actor: &Actor, config: &AccountConfig, caller_owner: Option<&str>) -> DResult<()> {
        let owner = &config.owner;
        let account = hub.mail_account_system(actor, owner, &config.account.label).await?;
        let Some(lease) = hub.acquire_mail_sync_lease_for(actor, owner, account.id, LEASE_SECONDS, caller_owner).await? else {
            return Ok(());
        };
        let result = tokio::time::timeout(PASS_TIMEOUT, async {
            // Read the cursor after acquiring the lease, not from a previous worker's snapshot.
            let current = hub.mail_account_system(actor, owner, &config.account.label).await.map_err(|_| "mail account unavailable")?;
            self.pull_mail(hub, actor, config, &current, lease).await
        })
        .await;
        let (status, cursor, error) = match result {
            Ok(Ok(Some(cursor))) => ("ok", Some(cursor), None),
            Ok(Ok(None)) => ("awaiting_credentials", None, None),
            Ok(Err(error)) => ("error", None, Some(error)),
            Err(_) => ("error", None, Some("mail sync timed out")),
        };
        let updated = hub
            .update_mail_account_sync_state(actor, owner, account.id, lease, MailSyncState { status, cursor: cursor.as_ref(), error })
            .await;
        let released = hub.release_mail_sync_lease(actor, owner, account.id, lease).await;
        updated?;
        released
    }

    async fn pull_mail(
        &self,
        hub: &Hub,
        actor: &Actor,
        config: &AccountConfig,
        account: &MailAccount,
        lease: Uuid,
    ) -> Result<Option<Value>, &'static str> {
        let transport = &config.account.config_json;
        let archive = self.mail.archive_raw_dir.as_ref().map(|dir| dir.join(account.id.to_string()));
        let save = |messages| async move {
            hub.ingest_mail_messages_system(actor, &account.owner, lease, &MailIngestRequest { account_id: account.id, messages })
                .await
                .map_err(|_| "mail batch could not be saved")
        };
        if account.transport == "export_dir" {
            let root = transport.export_dir.as_ref().ok_or("missing mail export directory")?;
            let mut incoming = mail_parse::stream_export(root.into(), 10_000, move |msg, raw| archive_raw(archive.as_deref(), msg, raw));
            let mut batch = Vec::new();
            while let Some(message) = incoming.recv().await {
                batch.push(message.map_err(|_| "mail export could not be read or archived")?);
                if batch.len() == 50 {
                    save(std::mem::take(&mut batch)).await?;
                }
            }
            if !batch.is_empty() {
                save(batch).await?;
            }
            return Ok(Some(account.sync_cursor_json.clone()));
        }
        let username = if config.username_credential.is_some() || config.username_file.is_some() {
            self.credential(config.username_credential.as_deref(), config.username_file.as_deref())
                .await
                .map_err(|_| "IMAP user credential unreadable")?
        } else {
            config.username.as_deref().map(str::trim).filter(|u| !u.is_empty()).map(str::to_owned).or_else(|| {
                let address = account.address.trim();
                (!address.is_empty()).then(|| address.to_owned())
            })
        };
        let password = self
            .credential(config.password_credential.as_deref(), config.password_file.as_deref())
            .await
            .map_err(|_| "IMAP password credential unreadable")?;
        let (Some(username), Some(password)) = (username, password) else {
            return Ok(None);
        };
        let password = if account.provider == "gmail" {
            imap_sync::normalize_gmail_app_password(&password)
        } else {
            std::borrow::Cow::Borrowed(password.as_str())
        };
        let host = transport.host.as_deref().ok_or("missing IMAP host")?;
        let tls: TlsMode = transport.tls_mode.as_deref().unwrap_or("tls").parse().map_err(|_| "invalid IMAP TLS mode")?;
        let port = transport.port.unwrap_or(if tls == TlsMode::Tls { 993 } else { 143 });
        let folders = if transport.folders.is_empty() { vec!["INBOX".into()] } else { transport.folders.clone() };
        let (messages, cursor) = imap_sync::sync_imap(
            &Connection { host, port, tls, username: &username, password: &password },
            &folders,
            &account.sync_cursor_json,
            move |msg, raw| archive_raw(archive.as_deref(), msg, raw),
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                owner = %account.owner,
                label = %account.label,
                host,
                error_category = error.category(),
                error = error.message(),
                "mail IMAP sync failed"
            );
            error.message()
        })?;
        let mut messages = messages.into_iter().peekable();
        while messages.peek().is_some() {
            save(messages.by_ref().take(50).collect()).await?;
        }
        Ok(Some(cursor))
    }

    async fn sync_calendar(&self, hub: &Hub, owner: Option<&str>) {
        let actor = Actor::system();
        for config in self.calendar.sources.iter().filter(|c| c.source.enabled && owner.is_none_or(|owner| owner == c.owner)) {
            if let Err(error) = self.sync_source(hub, &actor, config).await {
                tracing::warn!(code = error.code(), "calendar sync state could not be updated");
            }
        }
    }

    async fn sync_source(&self, hub: &Hub, actor: &Actor, config: &SourceConfig) -> DResult<()> {
        let owner = &config.owner;
        let source = hub.calendar_source_system(actor, owner, &config.source.label).await?;
        let Some(lease) = hub.acquire_calendar_sync_lease(actor, owner, source.id, LEASE_SECONDS).await? else {
            return Ok(());
        };
        let fetched = tokio::time::timeout(PASS_TIMEOUT, async {
            let current = hub
                .calendar_source_system(actor, owner, &config.source.label)
                .await
                .map_err(|_| io::Error::other("calendar source unavailable"))?;
            self.pull_calendar(config, current.sync_etag.as_deref()).await
        })
        .await;
        match fetched {
            Ok(Ok(Some(Snapshot::Complete { events, etag }))) => {
                match hub.replace_calendar_source_events(actor, owner, source.id, lease, etag.as_deref(), &events).await {
                    Ok(_) => Ok(()),
                    Err(_) => hub.record_calendar_sync_error(actor, owner, source.id, lease, "calendar snapshot could not be saved").await,
                }
            }
            Ok(Ok(Some(Snapshot::Unchanged))) => hub.finish_calendar_sync(actor, owner, source.id, lease, true, None).await,
            Ok(Ok(None)) => hub.finish_calendar_sync(actor, owner, source.id, lease, false, None).await,
            Ok(Err(error)) => hub.record_calendar_sync_error(actor, owner, source.id, lease, &error.to_string()).await,
            Err(_) => hub.record_calendar_sync_error(actor, owner, source.id, lease, "calendar sync timed out").await,
        }
    }

    async fn pull_calendar(&self, config: &SourceConfig, etag: Option<&str>) -> io::Result<Option<Snapshot>> {
        if config.source.provider == "google_api" {
            let Some(credentials) =
                self.credential(config.service_account_credential.as_deref(), config.service_account_file.as_deref()).await?
            else {
                return Ok(None);
            };
            let id = config.calendar_id.as_deref().ok_or_else(|| io::Error::other("missing Google calendar ID"))?;
            return calendar_sync::fetch_google_calendar_api(&self.client, &credentials, id).await.map(Some);
        }
        let url = if config.url_credential.is_some() || config.url_file.is_some() {
            self.credential(config.url_credential.as_deref(), config.url_file.as_deref()).await?
        } else {
            config.url.clone()
        };
        let Some(url) = url else {
            return Ok(None);
        };
        let mut auth = None;
        let mut basic = None;
        let mut bearer = None;
        if config.source.provider == "caldav" || config.password_credential.is_some() || config.password_file.is_some() {
            let user = if config.username_credential.is_some() || config.username_file.is_some() {
                self.credential(config.username_credential.as_deref(), config.username_file.as_deref()).await?
            } else {
                config.username.clone()
            };
            let pass = self.credential(config.password_credential.as_deref(), config.password_file.as_deref()).await?;
            let (Some(user), Some(pass)) = (user, pass) else {
                return Ok(None);
            };
            if config.source.provider == "caldav" {
                return calendar_sync::fetch_caldav_source(&self.client, &url, &user, &pass).await.map(Some);
            }
            basic = Some((user, pass));
        } else if config.bearer_token_credential.is_some() || config.bearer_token_file.is_some() {
            bearer = self.credential(config.bearer_token_credential.as_deref(), config.bearer_token_file.as_deref()).await?;
            if bearer.is_none() {
                return Ok(None);
            }
        }
        if let Some((user, password)) = &basic {
            auth = Some(Auth::Basic(user, password));
        }
        if let Some(token) = &bearer {
            auth = Some(Auth::Bearer(token));
        }
        calendar_sync::fetch_ics_source(&self.client, &url, auth, etag).await.map(Some)
    }
}

fn interval(seconds: u64) -> tokio::time::Interval {
    let mut tick = tokio::time::interval(Duration::from_secs(seconds.clamp(1, 86400)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
}

/// Read on each sync, so provisioning/replacing a fallback file needs no Hub restart.
async fn read_runtime_credential(directory: Option<&FsPath>, name: Option<&str>, fallback: Option<&FsPath>) -> io::Result<Option<String>> {
    const MAX_CREDENTIAL_BYTES: u64 = 256 * 1024;
    if name.is_some_and(|name| {
        name.is_empty() || matches!(name, "." | "..") || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    }) {
        return Err(io::Error::other("invalid runtime credential name"));
    }
    let preferred = directory.zip(name).map(|(dir, name)| dir.join(name));
    for path in preferred.as_deref().into_iter().chain(fallback) {
        let file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(io::Error::other("runtime credential unreadable")),
        };
        let mut value = String::new();
        file.take(MAX_CREDENTIAL_BYTES + 1)
            .read_to_string(&mut value)
            .await
            .map_err(|_| io::Error::other("runtime credential unreadable"))?;
        if value.len() as u64 > MAX_CREDENTIAL_BYTES {
            return Err(io::Error::other("runtime credential exceeds size limit"));
        }
        let value = value.trim();
        return Ok((!value.is_empty()).then(|| value.to_owned()));
    }
    Ok(None)
}

fn archive_raw(dir: Option<&FsPath>, msg: &MailMessageIn, raw: &[u8]) -> io::Result<Option<String>> {
    use std::io::Write;
    let Some(dir) = dir else {
        return Ok(None);
    };
    let hash = msg.content_sha256.as_ref().ok_or_else(|| io::Error::other("missing message content hash"))?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{hash}.eml"));
    if !path.try_exists()? {
        let temp = dir.join(format!(".{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new();
            file.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                file.mode(0o600);
            }
            let mut file = file.open(&temp)?;
            file.write_all(raw)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(temp);
            return Err(error);
        }
    }
    Ok(Some(path.to_string_lossy().into_owned()))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/mail/accounts", get(accounts))
        .route("/mail/ingest", post(ingest).layer(DefaultBodyLimit::max(32 * 1024 * 1024)))
        .route("/mail/sync", post(mail_sync))
        .route("/mail/messages", get(messages))
        .route("/mail/messages/{id}", get(message))
        .route("/mail/messages/{id}/triage", post(triage))
        .route("/mail/bi/overview", get(overview))
        .route("/mail/bi/digests", get(digests).post(generate_digest))
        .route("/calendar/sources", get(sources))
        .route("/calendar/sync", post(calendar_sync))
        .route("/calendar/events", get(events))
        .route("/calendar/agenda", get(agenda))
}

async fn accounts(State(state): State<AppState>, Caller(actor): Caller) -> R<Vec<MailAccount>> {
    call!(actor, state.hub.list_mail_accounts(&actor))
}

fn err(actor: &Actor) -> impl Fn(DomainError) -> ApiErr + '_ {
    move |error| match error {
        // SQL and decoding diagnostics can contain mail text or calendar data.
        DomainError::Db(_) | DomainError::Internal(_) => {
            tracing::error!(code = error.code(), correlation_id = ?actor.correlation_id, "mail/calendar storage error");
            ApiErr::coded(StatusCode::INTERNAL_SERVER_ERROR, error.code(), "mail/calendar storage error", actor.correlation_id.clone())
        }
        error => ApiErr::domain(error, actor.correlation_id.clone()),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Ingest {
    Id(MailIngestRequest),
    Named(MailNamedIngestRequest),
}

async fn ingest(State(state): State<AppState>, Caller(actor): Caller, Json(body): Json<Ingest>) -> R<MailIngestResult> {
    match body {
        Ingest::Id(body) => call!(actor, state.hub.ingest_mail_messages(&actor, &body)),
        Ingest::Named(body) => call!(actor, state.hub.ingest_named_mail_messages(&actor, body)),
    }
}

async fn mail_sync(State(state): State<AppState>, Caller(actor): Caller) -> R<Vec<MailAccount>> {
    let owner = state.hub.resolve_owner(&actor).map_err(err(&actor))?;
    let human = matches!(actor.principal.role, Role::Operator | Role::Member);
    state.mail_calendar.sync_mail(&state.hub, Some((&owner, human))).await;
    call!(actor, state.hub.list_mail_accounts(&actor))
}

async fn messages(
    State(state): State<AppState>,
    Caller(actor): Caller,
    Query(query): Query<MailSearchQuery>,
) -> R<Vec<MailMessageSummary>> {
    call!(actor, state.hub.list_mail_messages(&actor, &query))
}

async fn message(State(state): State<AppState>, Caller(actor): Caller, Path(id): Path<Uuid>) -> R<MailMessageDetail> {
    call!(actor, state.hub.get_mail_message(&actor, id))
}

async fn triage(
    State(state): State<AppState>,
    Caller(actor): Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<MailTriageOverride>,
) -> R<MailMessageDetail> {
    call!(actor, state.hub.override_mail_triage(&actor, id, &body))
}

async fn overview(State(state): State<AppState>, Caller(actor): Caller) -> R<MailBiOverview> {
    call!(actor, state.hub.mail_bi_overview(&actor))
}

async fn digests(State(state): State<AppState>, Caller(actor): Caller) -> R<Vec<MailBiDigest>> {
    call!(actor, state.hub.list_mail_bi_digests(&actor))
}

#[derive(Deserialize)]
struct DigestRequest {
    period_kind: Option<String>,
    period_start: Option<DateTime<Utc>>,
    period_end: Option<DateTime<Utc>>,
}

async fn generate_digest(State(state): State<AppState>, Caller(actor): Caller, body: Option<Json<DigestRequest>>) -> R<MailBiDigest> {
    let Json(body) = body.unwrap_or(Json(DigestRequest { period_kind: None, period_start: None, period_end: None }));
    let to = body.period_end.unwrap_or_else(Utc::now);
    let from = body.period_start.or_else(|| to.checked_sub_signed(chrono::Duration::days(7))).ok_or_else(|| {
        ApiErr::coded(StatusCode::UNPROCESSABLE_ENTITY, "mail_digest_period", "invalid digest period", actor.correlation_id.clone())
    })?;
    let kind = body.period_kind.as_deref().unwrap_or("weekly");
    let digest = state.hub.create_mail_bi_digest(&actor, kind, from, to).await.map_err(err(&actor))?;
    if let Some(llm) = state.mail_calendar.llm()
        && let Ok(summary) = local_llm_bi::digest(&state.mail_calendar.client, &llm.base_url, &llm.model, &digest.summary_markdown).await
    {
        return call!(actor, state.hub.summarize_mail_bi_digest(&actor, digest.id, &llm.model, &summary));
    }
    Ok(Json(digest))
}

async fn sources(State(state): State<AppState>, Caller(actor): Caller) -> R<Vec<CalendarSource>> {
    call!(actor, state.hub.list_calendar_sources(&actor))
}

async fn calendar_sync(State(state): State<AppState>, Caller(actor): Caller) -> R<Vec<CalendarSource>> {
    let owner = state.hub.resolve_owner(&actor).map_err(err(&actor))?;
    state.mail_calendar.sync_calendar(&state.hub, Some(&owner)).await;
    call!(actor, state.hub.list_calendar_sources(&actor))
}

#[derive(Deserialize)]
struct TimeRange {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

async fn events(State(state): State<AppState>, Caller(actor): Caller, Query(range): Query<TimeRange>) -> R<Vec<CalendarEvent>> {
    call!(actor, state.hub.list_calendar_events(&actor, range.from, range.to))
}

#[derive(Deserialize)]
struct AgendaQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    days: Option<u32>,
}

async fn agenda(State(state): State<AppState>, Caller(actor): Caller, Query(query): Query<AgendaQuery>) -> R<CalendarAgendaResponse> {
    let days = query.days.unwrap_or(14);
    let from = query.from.unwrap_or_else(Utc::now);
    let to = query.to.or_else(|| from.checked_add_signed(chrono::Duration::days(days.into())));
    let to = to.filter(|_| (1..=366).contains(&days)).ok_or_else(|| {
        ApiErr::coded(StatusCode::UNPROCESSABLE_ENTITY, "calendar_range", "days must be between 1 and 366", actor.correlation_id.clone())
    })?;
    call!(actor, state.hub.calendar_agenda(&actor, from, to))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_credential_returns_none_for_missing_empty_or_unconfigured_files() {
        let dir = std::env::temp_dir().join(format!("fleetix-hub-cred-{}", Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        assert_eq!(read_runtime_credential(None, None, None).await.unwrap(), None);

        let missing = dir.join("missing-password");
        assert_eq!(read_runtime_credential(None, None, Some(&missing)).await.unwrap(), None);

        let empty = dir.join("empty-password");
        tokio::fs::write(&empty, "  \n\t\r\n").await.unwrap();
        assert_eq!(read_runtime_credential(None, None, Some(&empty)).await.unwrap(), None);

        let populated = dir.join("gmail-password");
        tokio::fs::write(&populated, "abcd efgh ijkl mnop\n").await.unwrap();
        assert_eq!(read_runtime_credential(None, None, Some(&populated)).await.unwrap().as_deref(), Some("abcd efgh ijkl mnop"));

        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }
}
