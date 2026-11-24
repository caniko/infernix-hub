//! Incremental read-only retrieval of message bodies over IMAP.

use crate::mail_parse::{MAX_RAW_BYTES, parse_eml};
use contracts::{MailImapLocationIn, MailMessageIn};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

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

fn quote(value: &str) -> io::Result<String> {
    if value.chars().any(char::is_control) {
        return Err(invalid("IMAP argument contains control characters"));
    }
    Ok(format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
type Imap = BufReader<Box<dyn Stream>>;

async fn tls(socket: Box<dyn Stream>, host: &str) -> io::Result<Box<dyn Stream>> {
    let name = ServerName::try_from(host.to_owned()).map_err(|_| invalid("invalid IMAP TLS server name"))?;
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| invalid("invalid IMAP TLS configuration"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Box::new(TlsConnector::from(Arc::new(config)).connect(name, socket).await.map_err(|_| invalid("IMAP TLS handshake failed"))?))
}

async fn line(socket: &mut Imap) -> io::Result<String> {
    const MAX_LINE: usize = 4 * 1024 * 1024;
    let mut bytes = Vec::new();
    socket.take(MAX_LINE as u64).read_until(b'\n', &mut bytes).await?;
    if !bytes.ends_with(b"\r\n") {
        return Err(invalid("incomplete or oversized IMAP response line"));
    }
    bytes.truncate(bytes.len() - 2);
    String::from_utf8(bytes).map_err(|_| invalid("invalid IMAP response line"))
}

async fn command(
    socket: &mut Imap,
    sequence: &mut u64,
    cmd: &str,
    mut response: impl FnMut(&str, Option<&[u8]>) -> io::Result<()>,
) -> io::Result<()> {
    // Only one command is outstanding at a time.
    *sequence += 1;
    let tag = format!("H{sequence} ");
    socket.write_all(format!("{tag}{cmd}\r\n").as_bytes()).await?;
    socket.flush().await?;
    loop {
        let mut text = line(socket).await?;
        if let Some(status) = text.strip_prefix(tag.as_str()) {
            return if status.split_ascii_whitespace().next().is_some_and(|s| s.eq_ignore_ascii_case("OK")) {
                Ok(())
            } else {
                Err(invalid("IMAP command rejected"))
            };
        }
        if text.to_ascii_uppercase().starts_with("* BYE") {
            return Err(invalid("IMAP server closed the session"));
        }
        let mut literal = None;
        if let Some(length) = text.strip_suffix('}').and_then(|s| s.rsplit_once('{')).map(|(_, n)| n) {
            let length =
                length.parse::<usize>().ok().filter(|n| *n <= MAX_RAW_BYTES).ok_or_else(|| invalid("invalid or oversized IMAP literal"))?;
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await?;
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
) -> io::Result<(Vec<MailMessageIn>, Value)> {
    validate_transport(connection.host, connection.tls)?;
    if folders.len() > 16 || folders.iter().any(|f| f.is_empty() || f.len() > 1024) {
        return Err(invalid("IMAP folder list exceeds limits or contains an empty folder"));
    }
    timeout(Duration::from_secs(120), async {
        let host = connection.host;
        let port = connection.port;
        let tcp = if connection.tls == TlsMode::Plain && host == "localhost" {
            match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(socket) => socket,
                Err(_) => TcpStream::connect(("::1", port)).await?,
            }
        } else {
            TcpStream::connect((host, port)).await?
        };
        let socket: Box<dyn Stream> = Box::new(tcp);
        let mut socket = BufReader::new(if connection.tls == TlsMode::Tls { tls(socket, host).await? } else { socket });
        let mut sequence = 0;
        if !line(&mut socket).await?.to_ascii_uppercase().starts_with("* OK") {
            return Err(invalid("invalid IMAP server greeting"));
        }
        if connection.tls == TlsMode::Starttls {
            command(&mut socket, &mut sequence, "STARTTLS", |_, _| Ok(())).await?;
            if !socket.buffer().is_empty() {
                return Err(invalid("unexpected data before IMAP TLS upgrade"));
            }
            socket = BufReader::new(tls(socket.into_inner(), host).await?);
        }
        command(&mut socket, &mut sequence, &format!("LOGIN {} {}", quote(connection.username)?, quote(connection.password)?), |_, _| {
            Ok(())
        })
        .await?;
        let mut cursors: BTreeMap<String, FolderCursor> = if cursor.is_null() {
            BTreeMap::new()
        } else {
            serde_json::from_value(cursor.clone()).map_err(|_| invalid("invalid IMAP cursor"))?
        };
        let mut messages = Vec::new();
        for folder in folders {
            let mut validity = None;
            command(&mut socket, &mut sequence, &format!("SELECT {}", quote(folder)?), |line, _| {
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
            let validity = validity.ok_or_else(|| invalid("IMAP SELECT omitted UIDVALIDITY"))?;
            let cursor = cursors.entry(folder.clone()).or_default();
            if cursor.uidvalidity != validity {
                *cursor = FolderCursor { uidvalidity: validity, last_uid: 0 };
            }
            let Some(start) = cursor.last_uid.checked_add(1) else { continue };
            let mut uids = BTreeSet::new();
            command(&mut socket, &mut sequence, &format!("UID SEARCH UID {start}:*"), |line, _| {
                let mut fields = line.split_ascii_whitespace();
                if fields.next() == Some("*") && fields.next().is_some_and(|s| s.eq_ignore_ascii_case("SEARCH")) {
                    for uid in fields {
                        let uid: u32 = uid.parse().map_err(|_| invalid("invalid IMAP search UID"))?;
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
            command(&mut socket, &mut sequence, &format!("UID FETCH {set} (UID FLAGS BODY.PEEK[])"), |line, raw| {
                let Some(raw) = raw else { return Ok(()) };
                let text = line.to_ascii_uppercase();
                if !text.starts_with("* ") || !text.contains(" FETCH (") || !text.contains("BODY[] {") {
                    return Err(invalid("unexpected IMAP FETCH literal"));
                }
                let uid = text
                    .split_once(" UID ")
                    .or_else(|| text.split_once("(UID "))
                    .and_then(|(_, u)| u.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u32>().ok())
                    .filter(|u| uids.contains(u) && fetched.insert(*u))
                    .ok_or_else(|| invalid("missing or unexpected IMAP FETCH UID"))?;
                let mut message = parse_eml(raw, folder)?;
                message.is_read = text
                    .split_once("FLAGS (")
                    .and_then(|(_, f)| f.split_once(')'))
                    .is_some_and(|(f, _)| f.split_ascii_whitespace().any(|flag| flag == "\\SEEN"));
                message.imap_location = Some(MailImapLocationIn { folder: folder.clone(), uidvalidity: validity.into(), uid: uid.into() });
                message.raw_path = archive(&message, raw)?;
                messages.push(message);
                Ok(())
            })
            .await?;
            if fetched.len() != uids.len() {
                // A concurrent expunge can cause this; retry SEARCH on the next run.
                return Err(invalid("IMAP FETCH returned an incomplete batch"));
            }
            if let Some(last) = uids.last() {
                cursor.last_uid = *last;
            }
        }
        let cursors = serde_json::to_value(cursors).map_err(|_| invalid("cannot encode IMAP cursor"))?;
        Ok((messages, cursors))
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IMAP sync timed out"))?
}
