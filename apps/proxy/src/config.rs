use std::{
    collections::BTreeSet,
    env, fs,
    io::{ErrorKind, Write},
    net::SocketAddr,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, ensure};
use reqwest::Url;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::pool::Lane;

pub const DEFAULT_BIND: &str = "127.0.0.1:8317";
pub const DEFAULT_URL: &str = "http://127.0.0.1:8317";

pub struct Config {
    pub bind: SocketAddr,
    pub home: PathBuf,
    pub t3code_home: PathBuf,
    pub auth_dir: PathBuf,
    pub discover_local: bool,
    pub exclude_local_lanes: BTreeSet<Lane>,
    pub provenance_oauth_dir: PathBuf,
    pub provenance_oauth_bin: Option<PathBuf>,
    pub client_key: String,
    pub management_key: String,
    pub anthropic: Url,
    pub openai: Url,
    pub codex: Url,
    pub antigravity: Url,
    pub url: String,
}

impl Config {
    pub fn new(
        bind: SocketAddr,
        state: &Path,
        auth_dir: Option<PathBuf>,
        discover_local: bool,
        client_file: Option<PathBuf>,
        management_file: Option<PathBuf>,
    ) -> Result<Self> {
        private_dir(state)?;
        let auth_dir = auth_dir.unwrap_or_else(|| state.join("auth"));
        private_dir(&auth_dir)?;
        let client_key = key(state, "client-key", "PROXY_CLIENT_KEY", client_file.as_deref(), true)?;
        let management_key = key(state, "management-key", "PROXY_MANAGEMENT_KEY", management_file.as_deref(), true)?;
        ensure!(!matches_secret(&client_key, &management_key), "client and management keys must differ");
        let upstream = |name, default| origin(&env::var(name).unwrap_or_else(|_| String::from(default)));
        let home = home()?;
        Ok(Self {
            bind,
            t3code_home: t3_home(&home),
            home,
            auth_dir,
            discover_local,
            exclude_local_lanes: BTreeSet::new(),
            provenance_oauth_dir: env::var_os("PROXY_PROVENANCE_OAUTH_DIR")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/provenance/oauth")),
            provenance_oauth_bin: env::var_os("PROXY_PROVENANCE_OAUTH_BIN").filter(|p| !p.is_empty()).map(PathBuf::from),
            client_key,
            management_key,
            anthropic: upstream("PROXY_ANTHROPIC_UPSTREAM", "https://api.anthropic.com")?,
            openai: upstream("PROXY_OPENAI_UPSTREAM", "https://api.openai.com")?,
            codex: upstream("PROXY_OPENAI_UPSTREAM", "https://chatgpt.com/backend-api/codex")?,
            antigravity: upstream("PROXY_ANTIGRAVITY_UPSTREAM", "https://daily-cloudcode-pa.googleapis.com")?,
            url: format!("http://{bind}"),
        })
    }
}

pub fn home() -> Result<PathBuf> {
    env::var_os("HOME").filter(|p| !p.is_empty()).map(PathBuf::from).context("HOME is not set")
}

pub fn state_dir() -> Result<PathBuf> {
    Ok(env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or(home()?.join(".local/state")).join("fleetix-agent-proxy"))
}

pub fn t3_home(home: &Path) -> PathBuf {
    env::var_os("T3CODE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share/t3code-native"))
}

pub fn origin(input: &str) -> Result<Url> {
    let url = Url::parse(input).map_err(|_| anyhow!("invalid endpoint URL"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost" || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "endpoint must use HTTPS or loopback HTTP without credentials, query, or fragment"
    );
    Ok(url)
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    Ok(())
}

pub fn read_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub fn secret_file(path: &Path) -> Result<String> {
    validate_secret(&read_text(path)?.context("secret file not found")?)
}

pub(crate) fn validate_secret(raw: &str) -> Result<String> {
    let secret = raw.trim();
    ensure!(!secret.is_empty() && secret.bytes().all(|b| b.is_ascii_graphic()), "invalid secret");
    Ok(secret.to_owned())
}

pub fn key(state: &Path, name: &str, env_name: &str, file: Option<&Path>, create: bool) -> Result<String> {
    if let Some(path) = file {
        return secret_file(path);
    }
    if let Ok(value) = env::var(env_name) {
        return validate_secret(&value);
    }
    let path = state.join(name);
    if !create {
        return secret_file(&path);
    }
    let mut bytes = [0; 32];
    SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow!("could not generate key"))?;
    write_private(&path, hex::encode(bytes).as_bytes(), false)?;
    ensure!(
        fs::symlink_metadata(&path)?.file_type().is_file() && fs::metadata(&path)?.permissions().mode() & 0o777 == 0o600,
        "generated key file must be a regular file with mode 0600"
    );
    secret_file(&path)
}

/// Publish an already synced 0600 file. A backup/key creation must never replace a winner.
pub fn write_private(path: &Path, data: &[u8], replace: bool) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    private_dir(dir)?;
    let temp = dir.join(format!(".fleetix-{}.tmp", Uuid::new_v4()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        if replace {
            fs::rename(&temp, path)?;
        } else if let Err(e) = fs::hard_link(&temp, path)
            && e.kind() != ErrorKind::AlreadyExists
        {
            return Err(e);
        }
        fs::File::open(dir)?.sync_all()
    })();
    let _ = fs::remove_file(&temp);
    result.with_context(|| format!("publish {}", path.display()))
}

pub fn json_object(text: &str) -> Result<Map<String, Value>> {
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_str(text).map_err(|_| anyhow!("settings must be a valid JSON object"))
}

pub fn matches_secret(left: &str, right: &str) -> bool {
    use ring::hmac::{HMAC_SHA256, Key, sign, verify};
    let key = Key::new(HMAC_SHA256, b"fleetix agent proxy authentication");
    verify(&key, right.as_bytes(), sign(&key, left.as_bytes()).as_ref()).is_ok()
}
