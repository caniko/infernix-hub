//! Incremental read-only retrieval of message bodies over IMAP.

use crate::mail_parse::{MAX_RAW_BYTES, parse_eml};
use contracts::{MailImapLocationIn, MailMessageIn};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapSyncError {
    InvalidConfig(&'static str),
    ConnectionFailed,
    TlsHandshakeFailed,
    ServerGreetingFailed,
    StarttlsFailed,
    AuthenticationFailed,
    FolderSelectionFailed,
    SearchFailed,
    FetchFailed,
    MessageParseFailed,
    ArchiveFailed,
    ProtocolError(&'static str),
    TimedOut,
}

impl ImapSyncError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidConfig(message) | Self::ProtocolError(message) => message,
            Self::ConnectionFailed => "IMAP connection failed",
            Self::TlsHandshakeFailed => "IMAP TLS handshake failed",
            Self::ServerGreetingFailed => "IMAP server greeting failed",
            Self::StarttlsFailed => "IMAP STARTTLS failed",
            Self::AuthenticationFailed => "IMAP authentication failed",
            Self::FolderSelectionFailed => "IMAP folder selection failed",
            Self::SearchFailed => "IMAP message search failed",
            Self::FetchFailed => "IMAP message fetch failed",
            Self::MessageParseFailed => "IMAP message parsing failed",
            Self::ArchiveFailed => "IMAP message archive failed",
            Self::TimedOut => "IMAP sync timed out",
        }
    }

    pub const fn category(self) -> &'static str {
        match self {
            Self::InvalidConfig(_) => "invalid_config",
            Self::ConnectionFailed => "connection",
            Self::TlsHandshakeFailed => "tls",
            Self::ServerGreetingFailed => "greeting",
            Self::StarttlsFailed => "starttls",
            Self::AuthenticationFailed => "authentication",
            Self::FolderSelectionFailed => "folder_selection",
            Self::SearchFailed => "search",
            Self::FetchFailed => "fetch",
            Self::MessageParseFailed => "parse",
            Self::ArchiveFailed => "archive",
            Self::ProtocolError(_) => "protocol",
            Self::TimedOut => "timeout",
        }
    }
}

impl fmt::Display for ImapSyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ImapSyncError {}

/// Strip display spaces from a 16-character Google app password (`xxxx xxxx xxxx xxxx`).
pub fn normalize_gmail_app_password(password: &str) -> Cow<'_, str> {
    let bytes = password.as_bytes();
    if bytes.len() == 19
        && bytes[4] == b' '
        && bytes[9] == b' '
        && bytes[14] == b' '
        && bytes.iter().enumerate().all(|(i, b)| matches!(i, 4 | 9 | 14) || b.is_ascii_alphanumeric())
    {
        Cow::Owned(password.replace(' ', ""))
    } else {
        Cow::Borrowed(password)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    #[default]
    Tls,
    Starttls,
    Plain,
}

impl std::str::FromStr for TlsMode {
    type Err = io::Error;
    fn from_str(value: &str) -> io::Result<Self> {
        match value {
            "tls" => Ok(Self::Tls),
            "starttls" => Ok(Self::Starttls),
            "plain" => Ok(Self::Plain),
            _ => Err(invalid("invalid IMAP TLS mode")),
        }
    }
}

pub struct Connection<'a> {
    pub host: &'a str,
    pub port: u16,
    pub tls: TlsMode,
    pub username: &'a str,
    pub password: &'a str,
}

#[derive(Default, Serialize, Deserialize)]
struct FolderCursor {
    uidvalidity: u32,
    last_uid: u32,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn validate_transport(host: &str, tls: TlsMode) -> io::Result<()> {
    if tls == TlsMode::Plain && !matches!(host, "127.0.0.1" | "::1" | "localhost") {
        return Err(invalid("plain IMAP requires an explicit loopback host"));
    }
    Ok(())
}

fn quote(value: &str) -> Result<String, ImapSyncError> {
    if value.chars().any(char::is_control) {
        return Err(ImapSyncError::InvalidConfig("IMAP argument contains control characters"));
    }
    Ok(format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type Imap = BufReader<Box<dyn Stream>>;

async fn tls(socket: Box<dyn Stream>, host: &str) -> Result<Box<dyn Stream>, ImapSyncError> {
    let name = ServerName::try_from(host.to_owned()).map_err(|_| ImapSyncError::InvalidConfig("invalid IMAP TLS server name"))?;
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| ImapSyncError::InvalidConfig("invalid IMAP TLS configuration"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Box::new(TlsConnector::from(Arc::new(config)).connect(name, socket).await.map_err(|_| ImapSyncError::TlsHandshakeFailed)?))
}

async fn line(socket: &mut Imap) -> Result<String, ImapSyncError> {
    const MAX_LINE: usize = 4 * 1024 * 1024;
    let mut bytes = Vec::new();
    socket
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| ImapSyncError::ProtocolError("IMAP connection read failed"))?;
    if !bytes.ends_with(b"\r\n") {
        return Err(ImapSyncError::ProtocolError("incomplete or oversized IMAP response line"));
    }
    bytes.truncate(bytes.len() - 2);
    String::from_utf8(bytes).map_err(|_| ImapSyncError::ProtocolError("invalid IMAP response line"))
}

async fn command(
    socket: &mut Imap,
    sequence: &mut u64,
    cmd: &str,
    on_reject: ImapSyncError,
    mut response: impl FnMut(&str, Option<&[u8]>) -> Result<(), ImapSyncError>,
) -> Result<(), ImapSyncError> {
    // Only one command is outstanding at a time.
    *sequence += 1;
    let tag = format!("H{sequence} ");
    socket
        .write_all(format!("{tag}{cmd}\r\n").as_bytes())
        .await
        .map_err(|_| ImapSyncError::ProtocolError("IMAP connection write failed"))?;
    socket.flush().await.map_err(|_| ImapSyncError::ProtocolError("IMAP connection write failed"))?;
    loop {
        let mut text = line(socket).await?;
        if let Some(status) = text.strip_prefix(tag.as_str()) {
            return if status.split_ascii_whitespace().next().is_some_and(|s| s.eq_ignore_ascii_case("OK")) {
                Ok(())
            } else {
                Err(on_reject)
            };
        }
        if text.to_ascii_uppercase().starts_with("* BYE") {
            return Err(ImapSyncError::ProtocolError("IMAP server closed the session"));
        }
        let mut literal = None;
        if let Some(length) = text.strip_suffix('}').and_then(|s| s.rsplit_once('{')).map(|(_, n)| n) {
            let length = length
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= MAX_RAW_BYTES)
                .ok_or(ImapSyncError::ProtocolError("invalid or oversized IMAP literal"))?;
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.map_err(|_| ImapSyncError::ProtocolError("IMAP literal read failed"))?;
            text.push_str(&line(socket).await?);
            literal = Some(bytes);
        }
        response(&text, literal.as_deref())?;
    }
}

/// Fetch a bounded page per folder. Persist the returned cursor only after saving all messages.
/// `archive` receives the original bytes before they are discarded.
pub async fn sync_imap(
    connection: &Connection<'_>,
    folders: &[String],
    cursor: &Value,
    mut archive: impl FnMut(&MailMessageIn, &[u8]) -> io::Result<Option<String>> + Send,
) -> Result<(Vec<MailMessageIn>, Value), ImapSyncError> {
    validate_transport(connection.host, connection.tls)
        .map_err(|_| ImapSyncError::InvalidConfig("plain IMAP requires an explicit loopback host"))?;
    if folders.len() > 16 || folders.iter().any(|f| f.is_empty() || f.len() > 1024) {
        return Err(ImapSyncError::InvalidConfig("IMAP folder list exceeds limits or contains an empty folder"));
    }
    timeout(Duration::from_secs(120), async {
        let host = connection.host;
        let port = connection.port;
        let tcp = if connection.tls == TlsMode::Plain && host == "localhost" {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(socket) => socket,
                Err(_) => TcpStream::connect(("::1", port)).await.map_err(|_| ImapSyncError::ConnectionFailed)?,
            }
        } else {
            TcpStream::connect((host, port)).await.map_err(|_| ImapSyncError::ConnectionFailed)?
        };
        let socket: Box<dyn Stream> = Box::new(tcp);
        let mut socket = BufReader::new(if connection.tls == TlsMode::Tls { tls(socket, host).await? } else { socket });
        let mut sequence = 0;
        let greeting = line(&mut socket).await.map_err(|_| ImapSyncError::ServerGreetingFailed)?;
        if !greeting.to_ascii_uppercase().starts_with("* OK") {
            return Err(ImapSyncError::ServerGreetingFailed);
        }
        if connection.tls == TlsMode::Starttls {
            command(&mut socket, &mut sequence, "STARTTLS", ImapSyncError::StarttlsFailed, |_, _| Ok(())).await?;
            if !socket.buffer().is_empty() {
                return Err(ImapSyncError::StarttlsFailed);
            }
            socket = BufReader::new(tls(socket.into_inner(), host).await?);
        }
        command(
            &mut socket,
            &mut sequence,
            &format!("LOGIN {} {}", quote(connection.username)?, quote(connection.password)?),
            ImapSyncError::AuthenticationFailed,
            |_, _| Ok(()),
        )
        .await?;
        let mut cursors: BTreeMap<String, FolderCursor> = if cursor.is_null() {
            BTreeMap::new()
        } else {
            serde_json::from_value(cursor.clone()).map_err(|_| ImapSyncError::InvalidConfig("invalid IMAP cursor"))?
        };
        let mut messages = Vec::new();
        for folder in folders {
            let mut validity = None;
            command(&mut socket, &mut sequence, &format!("SELECT {}", quote(folder)?), ImapSyncError::FolderSelectionFailed, |line, _| {
                if let Some(value) = line
                    .to_ascii_uppercase()
                    .split_once("[UIDVALIDITY ")
                    .and_then(|(_, v)| v.split_once(']'))
                    .and_then(|(v, _)| v.parse::<u32>().ok())
                    .filter(|v| *v > 0)
                {
                    validity = Some(value);
                }
                Ok(())
            })
            .await?;
            let validity = validity.ok_or(ImapSyncError::FolderSelectionFailed)?;
            let cursor = cursors.entry(folder.clone()).or_default();
            if cursor.uidvalidity != validity {
                *cursor = FolderCursor { uidvalidity: validity, last_uid: 0 };
            }
            let Some(start) = cursor.last_uid.checked_add(1) else { continue };
            let mut uids = BTreeSet::new();
            command(&mut socket, &mut sequence, &format!("UID SEARCH UID {start}:*"), ImapSyncError::SearchFailed, |line, _| {
                let mut fields = line.split_ascii_whitespace();
                if fields.next() == Some("*") && fields.next().is_some_and(|s| s.eq_ignore_ascii_case("SEARCH")) {
                    for uid in fields {
                        let uid: u32 = uid.parse().map_err(|_| ImapSyncError::SearchFailed)?;
                        if uid >= start {
                            uids.insert(uid);
                            // ponytail: one bounded page per folder; repeat sync to drain large mailboxes.
                            if uids.len() > 50 {
                                uids.pop_last();
                            }
                        }
                    }
                }
                Ok(())
            })
            .await?;
            if uids.is_empty() {
                continue;
            }
            let set = uids.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
            let mut fetched = BTreeSet::new();
            command(
                &mut socket,
                &mut sequence,
                &format!("UID FETCH {set} (UID FLAGS BODY.PEEK[])"),
                ImapSyncError::FetchFailed,
                |line, raw| {
                    let Some(raw) = raw else { return Ok(()) };
                    let text = line.to_ascii_uppercase();
                    if !text.starts_with("* ") || !text.contains(" FETCH (") || !text.contains("BODY[] {") {
                        return Err(ImapSyncError::FetchFailed);
                    }
                    let uid = text
                        .split_once(" UID ")
                        .or_else(|| text.split_once("(UID "))
                        .and_then(|(_, u)| u.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u32>().ok())
                        .filter(|u| uids.contains(u) && fetched.insert(*u))
                        .ok_or(ImapSyncError::FetchFailed)?;
                    let mut message = parse_eml(raw, folder).map_err(|_| ImapSyncError::MessageParseFailed)?;
                    message.is_read = text
                        .split_once("FLAGS (")
                        .and_then(|(_, f)| f.split_once(')'))
                        .is_some_and(|(f, _)| f.split_ascii_whitespace().any(|flag| flag == "\\SEEN"));
                    message.imap_location =
                        Some(MailImapLocationIn { folder: folder.clone(), uidvalidity: validity.into(), uid: uid.into() });
                    message.raw_path = archive(&message, raw).map_err(|_| ImapSyncError::ArchiveFailed)?;
                    messages.push(message);
                    Ok(())
                },
            )
            .await?;
            if fetched.len() != uids.len() {
                // A concurrent expunge can cause this; retry SEARCH on the next run.
                return Err(ImapSyncError::FetchFailed);
            }
            if let Some(last) = uids.last() {
                cursor.last_uid = *last;
            }
        }
        let cursors = serde_json::to_value(cursors).map_err(|_| ImapSyncError::ProtocolError("cannot encode IMAP cursor"))?;
        Ok((messages, cursors))
    })
    .await
    .map_err(|_| ImapSyncError::TimedOut)?
}
