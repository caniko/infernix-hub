//! Parse local mail without fetching external resources.

use chrono::DateTime;
use contracts::MailMessageIn;
use mail_parser::{Address, MessageParser, MimeHeaders, PartType};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

pub const MAX_RAW_BYTES: usize = 15 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn field(text: &str, max_bytes: usize) -> String {
    // PostgreSQL text/JSONB does not store NUL. Hashes are still over raw bytes.
    text[..text.floor_char_boundary(max_bytes)].replace('\0', "")
}

pub fn parse_eml(raw: &[u8], folder: &str) -> io::Result<MailMessageIn> {
    if raw.len() > MAX_RAW_BYTES {
        return Err(invalid("RFC822 message exceeds 15 MiB"));
    }
    let mail = MessageParser::default().parse(raw).ok_or_else(|| invalid("invalid RFC822 message"))?;
    let address = mail.from().and_then(Address::first);
    let from_addr = field(address.and_then(|a| a.address()).unwrap_or_default(), 1024);
    let addresses = |header: Option<&Address>| {
        header.into_iter().flat_map(Address::iter).filter_map(|a| a.address().map(|addr| field(addr, 512))).take(256).collect()
    };
    let message_id_header = mail.message_id().map(|id| field(id, 2048));
    let root = mail.references().as_text_list().and_then(|v| v.first()).or_else(|| mail.in_reply_to().as_text_list()?.first());
    let thread_key = root.map(|id| field(id, 2048)).or_else(|| message_id_header.clone()).unwrap_or_else(|| {
        field(mail.thread_name().unwrap_or_default(), 2048).split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
    });
    // mail-parser supplies a text conversion for HTML-only MIME bodies.
    let plain = mail.text_bodies().find_map(|part| match &part.body {
        PartType::Text(text) => Some(text.as_ref()),
        _ => None,
    });
    let body_text = plain.map_or_else(|| field(&mail.body_text(0).unwrap_or_default(), MAX_BODY_BYTES), |body| field(body, MAX_BODY_BYTES));
    let mut attachments = mail.attachments().peekable();
    let has_attachments = attachments.peek().is_some();
    let attachment_names = attachments.filter_map(|p| p.attachment_name().map(|name| field(name, 1024))).take(128).collect();
    let safe_headers =
        ["list-unsubscribe", "precedence", "auto-submitted", "x-spam-flag", "x-spam-status", "x-mailer", "in-reply-to", "references"];
    let raw_headers = mail
        .headers_raw()
        .filter_map(|(key, value)| {
            let key = key.to_ascii_lowercase();
            safe_headers.contains(&key.as_str()).then(|| {
                let value = field(value, 4096);
                (key, value.split_whitespace().collect::<Vec<_>>().join(" "))
            })
        })
        .collect();
    Ok(MailMessageIn {
        message_id_header,
        thread_key,
        folder: folder.into(),
        from_domain: from_addr.rsplit_once('@').map(|(_, domain)| domain.to_ascii_lowercase()).unwrap_or_default(),
        from_addr,
        from_name: field(address.and_then(|a| a.name()).unwrap_or_default(), 1024),
        to_addrs: addresses(mail.to()),
        cc_addrs: addresses(mail.cc()),
        subject: field(mail.subject().unwrap_or_default(), 8192),
        sent_at: mail.date().and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0)),
        snippet: body_text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect(),
        body_text,
        has_attachments,
        attachment_names,
        raw_headers,
        size_bytes: raw.len() as i64,
        content_sha256: Some(hex::encode(Sha256::digest(raw))),
        ..Default::default()
    })
}

fn envelope(line: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(b"From ") else { return false };
    let mut fields = rest.split(|b| b.is_ascii_whitespace()).filter(|f| !f.is_empty());
    fields.next().is_some()
        && matches!(fields.next(), Some(b"Mon" | b"Tue" | b"Wed" | b"Thu" | b"Fri" | b"Sat" | b"Sun"))
        && fields.take(4).count() == 4
}

fn mbox_records(mut reader: impl BufRead) -> impl Iterator<Item = io::Result<Vec<u8>>> {
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let mut record = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.by_ref().take((MAX_RAW_BYTES + 1) as u64).read_until(b'\n', &mut line);
            match read {
                Ok(0) => {
                    done = true;
                    return (!record.is_empty()).then_some(Ok(record));
                }
                Err(e) => {
                    done = true;
                    return Some(Err(e));
                }
                Ok(_) if envelope(&line) => {
                    if record.is_empty() {
                        continue;
                    }
                    return Some(Ok(record));
                }
                Ok(_) => {
                    let quoted = line.iter().take_while(|&&b| b == b'>').count();
                    let line = if quoted > 0 && line[quoted..].starts_with(b"From ") { &line[1..] } else { &line };
                    if record.len() + line.len() > MAX_RAW_BYTES {
                        done = true;
                        return Some(Err(invalid("mbox message exceeds 15 MiB")));
                    }
                    record.extend_from_slice(line);
                }
            }
        }
    })
}

pub fn iter_mbox_messages(reader: impl BufRead, folder: &str) -> impl Iterator<Item = io::Result<MailMessageIn>> {
    let folder = folder.to_owned();
    mbox_records(reader).map(move |record| parse_eml(&record?, &folder))
}

/// Visit parsed messages one at a time, retaining raw bytes only for the callback.
pub fn visit_export(path: &Path, folder: &str, mut visit: impl FnMut(MailMessageIn, &[u8]) -> io::Result<()>) -> io::Result<()> {
    let file = File::open(path)?;
    let mut accept = |raw: Vec<u8>| visit(parse_eml(&raw, folder)?, &raw);
    match extension(path).as_deref() {
        Some("eml") => {
            let mut raw = Vec::new();
            file.take((MAX_RAW_BYTES + 1) as u64).read_to_end(&mut raw)?;
            accept(raw)
        }
        Some("mbox") => mbox_records(BufReader::new(file)).try_for_each(|raw| accept(raw?)),
        _ => Err(invalid("expected .eml or .mbox")),
    }
}

fn extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

pub fn export_files(root: &Path, max_files: usize) -> io::Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    if max_files == 0 {
        return Ok(files);
    }
    let root_type = fs::symlink_metadata(root)?.file_type();
    if root_type.is_file() {
        return if matches!(extension(root).as_deref(), Some("eml" | "mbox")) {
            Ok(vec![(root.to_owned(), "INBOX".into())])
        } else {
            Err(invalid("expected .eml or .mbox"))
        };
    }
    if !root_type.is_dir() {
        return Err(invalid("expected a regular mail export file or directory"));
    }
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_unstable_by_key(|e| e.file_name());
        for entry in entries {
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file() && matches!(extension(&path).as_deref(), Some("eml" | "mbox")) {
                let folder = path.parent().and_then(|p| p.strip_prefix(root).ok()).filter(|p| !p.as_os_str().is_empty());
                files.push((path.clone(), folder.map_or_else(|| "INBOX".into(), |p| p.to_string_lossy().into_owned())));
                if files.len() == max_files {
                    return Ok(files);
                }
            }
        }
    }
    Ok(files)
}

pub fn collect_export_dir(root: &Path, max_files: usize) -> io::Result<Vec<MailMessageIn>> {
    let mut messages = Vec::new();
    for (path, folder) in export_files(root, max_files)? {
        visit_export(&path, &folder, |message, _| {
            messages.push(message);
            Ok(())
        })?;
    }
    Ok(messages)
}

pub fn stream_export(
    root: PathBuf,
    max_files: usize,
    mut archive: impl FnMut(&MailMessageIn, &[u8]) -> io::Result<Option<String>> + Send + 'static,
) -> tokio::sync::mpsc::Receiver<io::Result<MailMessageIn>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    tokio::task::spawn_blocking(move || {
        let run = || {
            for (path, folder) in export_files(&root, max_files)? {
                visit_export(&path, &folder, |mut msg, raw| {
                    if sender.is_closed() {
                        return Err(invalid("mail export cancelled"));
                    }
                    msg.raw_path = archive(&msg, raw)?;
                    sender.blocking_send(Ok(msg)).map_err(|_| invalid("mail export cancelled"))
                })?;
            }
            Ok(())
        };
        let mut run = run;
        if let Err(error) = run() {
            let _ = sender.blocking_send(Err(error));
        }
    });
    receiver
}
