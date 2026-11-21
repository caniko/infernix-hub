use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Map, Value, json};

use crate::config::{json_object, origin, private_dir, read_text, write_private};

const BASE: &str = "ANTHROPIC_BASE_URL";
const HEADERS: &str = "ANTHROPIC_CUSTOM_HEADERS";
const TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
const BEGIN: &str = "# >>> spillway";
const END: &str = "# <<< spillway";
const DISABLED: &str = "# spillway disabled: ";

fn write_json(path: &Path, value: &Map<String, Value>, original: &str) -> Result<()> {
    let eol = if original.contains("\r\n") { "\r\n" } else { "\n" };
    let bom = if original.starts_with('\u{feff}') { "\u{feff}" } else { "" };
    let trailing = if original.trim().is_empty() || original.ends_with('\n') { eol } else { "" };
    let body = serde_json::to_string_pretty(value)?.replace('\n', eol);
    write_private(path, format!("{bom}{body}{trailing}").as_bytes(), true)
}

pub fn register_t3code(home: &Path, id: &str, label: &str, url: &str, key: &str) -> Result<()> {
    origin(url)?;
    ensure!(!id.trim().is_empty() && !label.trim().is_empty(), "source id and label must not be empty");
    let path = home.join("userdata/settings.json");
    let _lock = lock(&path)?;
    let original = read_text(&path)?.unwrap_or_default();
    let mut settings = json_object(&original)?;
    let sources = settings.entry("usageLimitSources").or_insert_with(|| json!({}));
    sources.as_object_mut().context("usageLimitSources must be an object")?.insert(
        id.into(),
        json!({
            "kind": "cliproxy", "label": label, "url": url, "managementKey": key, "enabled": true
        }),
    );
    write_json(&path, &settings, &original)
}

fn lock(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().context("missing settings directory")?;
    private_dir(dir)?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(".fleetix-agent-proxy.lock"))?;
    lock.lock()?;
    Ok(lock)
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Client {
    ClaudeCode,
    Codex,
}

impl Client {
    pub const ALL: [Self; 2] = [Self::ClaudeCode, Self::Codex];

    pub fn parse(id: &str) -> Result<Self> {
        match id {
            "claude-code" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            _ => Err(anyhow!("unknown client")),
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    fn path(self, home: &Path) -> PathBuf {
        let (var, dir, file) = match self {
            Self::ClaudeCode => ("CLAUDE_CONFIG_DIR", ".claude", "settings.json"),
            Self::Codex => ("CODEX_HOME", ".codex", "config.toml"),
        };
        env::var_os(var).map(PathBuf::from).unwrap_or_else(|| home.join(dir)).join(file)
    }

    pub fn status(self, home: &Path, url: &str, key: &str) -> contracts::providers::ClientWiringStatus {
        use contracts::providers::{ClientWiringState::*, ClientWiringStatus};
        let path = self.path(home);
        let backup_available = backup(&path).is_file();
        let state = match read_text(&path).and_then(|v| {
            let text = v.unwrap_or_default();
            match self {
                Self::ClaudeCode => {
                    let settings = claude_settings(&text)?;
                    let env = settings.get("env").and_then(Value::as_object);
                    Ok(if !backup_available {
                        NotApplied
                    } else if env.is_some_and(|env| {
                        env.get(BASE).and_then(Value::as_str) == Some(url)
                            && header_lines(env).any(|line| line == format!("X-Api-Key: {key}"))
                            && !env.contains_key(TOKEN)
                    }) {
                        Applied
                    } else {
                        Outdated
                    })
                }
                Self::Codex => {
                    let (_, blocks) = partition(&text)?;
                    Ok(if blocks == 0 {
                        NotApplied
                    } else if edit_codex(&text, Some(url))? == text {
                        Applied
                    } else {
                        Outdated
                    })
                }
            }
        }) {
            Ok(state) => state,
            Err(_) => Error,
        };
        ClientWiringStatus {
            client_id: self.id().into(),
            display_name: self.display_name().into(),
            state,
            backup_available,
            managed_by: (backup_available || state == Applied).then(|| "spillway".into()),
            detail: None,
            instructions: None,
        }
    }

    pub fn wire(self, home: &Path, target: Option<(&str, &str)>) -> Result<()> {
        self.wire_at(&self.path(home), target)
    }

    pub(crate) fn wire_at(self, path: &Path, target: Option<(&str, &str)>) -> Result<()> {
        if let Some((url, key)) = target {
            origin(url)?;
            if matches!(self, Self::ClaudeCode) {
                ensure!(!key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic()), "invalid client key");
            }
        }
        let _lock = lock(path)?;
        let backup_path = backup(path);
        let current = read_text(path)?;
        let text = current.as_deref().unwrap_or_default();
        let saved = read_text(&backup_path)?;
        match self {
            Self::ClaudeCode => {
                if target.is_none() && saved.is_none() {
                    return Ok(());
                }
                let mut settings = claude_settings(text)?;
                let mut env = settings.remove("env").and_then(|v| v.as_object().cloned()).unwrap_or_default();
                if let Some((url, key)) = target {
                    let headers = header_lines(&env)
                        .filter(|line| !line.split_once(':').is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("x-api-key")))
                        .map(str::to_owned)
                        .chain(std::iter::once(format!("X-Api-Key: {key}")))
                        .collect::<Vec<_>>()
                        .join("\n");
                    env.insert(BASE.into(), json!(url));
                    env.insert(HEADERS.into(), json!(headers));
                    env.remove(TOKEN);
                    write_private(&backup_path, text.as_bytes(), false)?;
                    settings.insert("env".into(), Value::Object(env));
                } else {
                    let old = claude_settings(saved.as_deref().unwrap_or_default())?;
                    let old_env = old.get("env").and_then(Value::as_object);
                    for name in [BASE, HEADERS, TOKEN] {
                        if let Some(v) = old_env.and_then(|e| e.get(name)) {
                            env.insert(name.into(), v.clone());
                        } else {
                            env.remove(name);
                        }
                    }
                    if env.is_empty() && old_env.is_none() {
                        if settings.is_empty() && saved.as_deref() == Some("") {
                            remove_existing(path)?;
                            return remove_existing(&backup_path);
                        }
                    } else {
                        settings.insert("env".into(), Value::Object(env));
                    }
                }
                write_json(path, &settings, text)?;
            }
            Self::Codex => {
                let output = edit_codex(text, target.map(|(url, _)| url))?;
                if target.is_some() {
                    write_private(&backup_path, text.as_bytes(), false)?;
                }
                if target.is_none() && output.trim().is_empty() && saved.as_deref() == Some("") {
                    remove_existing(path)?;
                } else if output != text
                    && (target.is_some()
                        || saved.is_some()
                        || partition(text)?.1 > 0
                        || text.lines().any(|line| line.starts_with(DISABLED)))
                {
                    write_private(path, output.as_bytes(), true)?;
                }
                let auth = path.with_file_name("auth.json");
                if read_text(&auth)?
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .is_some_and(|raw| raw.pointer("/tokens/refresh_token").and_then(Value::as_str) == Some("spillway-pool"))
                {
                    remove_existing(&auth)?;
                }
            }
        }
        if target.is_none() {
            remove_existing(&backup_path)?;
        }
        Ok(())
    }
}

fn remove_existing(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn backup(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".spillway-backup");
    name.into()
}

fn claude_settings(text: &str) -> Result<Map<String, Value>> {
    let settings = json_object(text)?;
    ensure!(settings.get("env").is_none_or(Value::is_object), "Claude settings env must be an object");
    Ok(settings)
}

fn header_lines(env: &Map<String, Value>) -> impl Iterator<Item = &str> {
    env.get(HEADERS).and_then(Value::as_str).unwrap_or_default().lines().filter(|line| !line.trim().is_empty())
}

fn partition(text: &str) -> Result<(Vec<&str>, usize)> {
    let mut inside = false;
    let mut count = 0;
    let mut outside = Vec::new();
    for line in text.lines() {
        if inside && line.trim() == END {
            inside = false;
        } else if !inside && line.trim().starts_with(BEGIN) {
            count += 1;
            inside = true;
        } else if !inside {
            outside.push(line);
        }
    }
    ensure!(!inside, "unclosed Spillway block in Codex settings");
    Ok((outside, count))
}

fn edit_codex(text: &str, url: Option<&str>) -> Result<String> {
    let (lines, _) = partition(text)?;
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut output = String::new();
    if let Some(url) = url {
        let endpoint = serde_json::to_string(&format!("{}/backend-api/codex", url.trim_end_matches('/')))?;
        output.push_str(&format!(
            "{BEGIN}: added by Spillway; remove it with Undo in the app{eol}openai_base_url = {endpoint}{eol}{END}{eol}"
        ));
    }
    let mut root = true;
    for line in lines {
        root &= !line.trim_start().starts_with('[');
        let owned = line.split_once('=').is_some_and(|(key, _)| {
            matches!(
                key.trim(),
                "model_provider"
                    | "openai_base_url"
                    | "\"model_provider\""
                    | "\"openai_base_url\""
                    | "'model_provider'"
                    | "'openai_base_url'"
            )
        });
        if url.is_some() && root && owned {
            output.push_str(DISABLED);
        }
        output.push_str(if url.is_none() { line.strip_prefix(DISABLED).unwrap_or(line) } else { line });
        output.push_str(eol);
    }
    Ok(output)
}
