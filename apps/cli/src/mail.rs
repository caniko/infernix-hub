use anyhow::{Context, Result};
use clap::{ArgAction, Subcommand};
use contracts::{MailIngestRequest, MailMessageIn, MailNamedIngestRequest, MailSearchQuery, MailTriageOverride, client::HubClient};
use integrations::{
    imap_sync::{self, Connection, TlsMode},
    mail_parse,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Subcommand)]
pub enum Command {
    Accounts,
    /// Parse local .eml and .mbox exports without transmitting raw mail.
    Import {
        #[arg(long)]
        account: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        address: Option<String>,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=100))]
        batch_size: u32,
        path: PathBuf,
    },
    /// Read messages from an IMAP server or loopback bridge without marking them read.
    SyncImap {
        #[arg(long)]
        account: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        port: u16,
        #[arg(long)]
        username: String,
        #[arg(long)]
        password_file: PathBuf,
        #[arg(long, default_value = "tls")]
        tls: TlsMode,
        #[arg(long, value_delimiter = ',', default_value = "INBOX")]
        folders: Vec<String>,
        #[arg(long, default_value = "protonmail")]
        provider: String,
    },
    Sync,
    Search {
        query: String,
        #[arg(long)]
        include_junk: bool,
        #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u32).range(1..=100))]
        limit: u32,
    },
    Bi {
        #[arg(long)]
        digest: bool,
    },
    Triage {
        message_id: Uuid,
        #[arg(long, action = ArgAction::Set, conflicts_with = "clear_junk")]
        junk: Option<bool>,
        #[arg(long, conflicts_with = "clear_class")]
        class: Option<String>,
        #[arg(long)]
        clear_junk: bool,
        #[arg(long)]
        clear_class: bool,
    },
}

pub async fn run(client: &HubClient, command: Command) -> Result<()> {
    match command {
        Command::Accounts => super::print(client.list_mail_accounts().await?),
        Command::Import { account, provider, address, batch_size, path } => {
            let account = account_id(client, &account, provider, address).await?;
            let mut stream = mail_parse::stream_export(path, usize::MAX, |_, _| Ok(None));
            let mut transfer = Transfer::new(client, account, batch_size as usize);
            while let Some(message) = stream.recv().await {
                transfer.push(message.context("reading mail export")?).await?;
            }
            transfer.finish().await?;
        }
        Command::SyncImap { account, host, port, username, password_file, tls, folders, provider } => {
            imap_sync::validate_transport(&host, tls)?;
            let password = std::fs::read_to_string(password_file).context("reading IMAP password file")?;
            let id = account_id(client, &account, Some(provider), Some(username.clone())).await?;
            let path = cursor_path(&account)?;
            let key = json!({"host":host,"port":port,"user":username,"tls":format!("{tls:?}"),"account_id":id});
            let stored = match tokio::fs::read(&path).await {
                Ok(data) => serde_json::from_slice::<Value>(&data).context("reading IMAP cursor JSON")?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
                Err(error) => return Err(error).context("reading IMAP cursor"),
            };
            let cursor = if stored["key"] == key { &stored["cursor"] } else { &Value::Null };
            let (messages, cursor) = imap_sync::sync_imap(
                &Connection { host: &host, port, tls, username: &username, password: password.trim_end_matches(['\r', '\n']) },
                &folders,
                cursor,
                |_, _| Ok(None),
            )
            .await?;
            let mut transfer = Transfer::new(client, id, 50);
            for message in messages {
                transfer.push(message).await?;
            }
            transfer.flush().await?;
            store_cursor(&path, &json!({"key":key, "cursor":cursor})).await?;
            super::print(transfer.summary);
        }
        Command::Sync => super::print(client.sync_mail().await?),
        Command::Search { query, include_junk, limit } => {
            super::print(
                client
                    .list_mail_messages(&MailSearchQuery { q: Some(query), include_junk, limit: Some(limit), ..Default::default() })
                    .await?,
            );
        }
        Command::Bi { digest: true } => super::print(client.post::<_, contracts::MailBiDigest>("/mail/bi/digests", &json!({})).await?),
        Command::Bi { digest: false } => super::print(client.mail_bi_overview().await?),
        Command::Triage { message_id, junk, class, clear_junk, clear_class } => {
            let body = MailTriageOverride {
                classification: if clear_class { Some(None) } else { class.map(Some) },
                is_junk: if clear_junk { Some(None) } else { junk.map(Some) },
            };
            super::print(client.override_mail_triage(message_id, &body).await?);
        }
    }
    Ok(())
}

async fn account_id(client: &HubClient, label: &str, provider: Option<String>, address: Option<String>) -> Result<Uuid> {
    let label = label.trim();
    if let Some(account) = client.list_mail_accounts().await?.into_iter().find(|a| a.label == label) {
        anyhow::ensure!(account.enabled, "mail account is disabled");
        return Ok(account.id);
    }
    client.ingest_named_mail_messages(&MailNamedIngestRequest { account: label.into(), provider, address, messages: Vec::new() }).await?;
    client.list_mail_accounts().await?.into_iter().find(|a| a.label == label).map(|a| a.id).context("mail account was not registered")
}

#[derive(Default, serde::Serialize)]
struct Summary {
    scanned: usize,
    inserted: usize,
    deduplicated: usize,
    junk: usize,
}

struct Transfer<'a> {
    client: &'a HubClient,
    request: MailIngestRequest,
    batch_size: usize,
    bytes: usize,
    summary: Summary,
}

impl<'a> Transfer<'a> {
    fn new(client: &'a HubClient, account_id: Uuid, batch_size: usize) -> Self {
        Self { client, request: MailIngestRequest { account_id, messages: Vec::new() }, batch_size, bytes: 0, summary: Summary::default() }
    }

    async fn push(&mut self, message: MailMessageIn) -> Result<()> {
        const BATCH_BYTES: usize = 16 * 1024 * 1024;
        let bytes = serde_json::to_vec(&message)?.len() + 1;
        anyhow::ensure!(bytes <= BATCH_BYTES, "encoded mail message exceeds batch size limit");
        if self.request.messages.len() == self.batch_size || self.bytes + bytes > BATCH_BYTES {
            self.flush().await?;
        }
        self.request.messages.push(message);
        self.bytes += bytes;
        self.summary.scanned += 1;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if !self.request.messages.is_empty() {
            let result = self.client.ingest_mail_messages(&self.request).await?;
            self.summary.inserted += result.inserted;
            self.summary.deduplicated += result.duplicates;
            self.summary.junk += result.junk;
            self.request.messages.clear();
            self.bytes = 0;
        }
        Ok(())
    }

    async fn finish(mut self) -> Result<()> {
        self.flush().await?;
        super::print(self.summary);
        Ok(())
    }
}

fn cursor_path(account: &str) -> Result<PathBuf> {
    let root = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("XDG_STATE_HOME or HOME is required for IMAP cursor storage")?;
    let key = if account.len() <= 128 && account.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) {
        account.into()
    } else {
        format!("~{}", hex::encode(Sha256::digest(account)))
    };
    Ok(root.join("fleetix-hub/imap-cursors").join(format!("{key}.json")))
}

async fn store_cursor(path: &Path, cursor: &Value) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("invalid cursor path")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, cursor)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
