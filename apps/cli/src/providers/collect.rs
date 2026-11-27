use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    net::IpAddr,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::Args;
use contracts::{client::HubClient, providers::*};
use integrations::providers::{
    antigravity::probe_antigravity_quota,
    claude::{probe_claude_oauth_usage, probe_meridian_quota},
    opencode::{probe_chatgpt_wham_usage, probe_opencode_go_usage},
};
use reqwest::{Client, Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};

const PROVIDERS: [(&str, &str, &str); 3] =
    [("claude", "Claude", "claude-code"), ("antigravity", "Antigravity", "google"), ("opencode", "OpenCode", "openai")];
const REFRESH: ProviderCapability = ProviderCapability::RefreshQuota;
const OVERRIDE: ProviderCapability = ProviderCapability::SetPoolOverride;
const MAX_INPUT: u64 = 2 * 1024 * 1024;

#[derive(Args)]
pub(crate) struct Options {
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    user: Option<String>,
    /// Loopback Meridian; uses MERIDIAN_API_KEY if set.
    #[arg(long)]
    meridian_url: Option<String>,
    #[arg(long, requires = "proxy_management_key_file")]
    proxy_url: Option<String>,
    #[arg(long, requires = "proxy_url")]
    proxy_management_key_file: Option<PathBuf>,
    /// JSON render of Canix's Accounts.pkl (no credential material).
    #[arg(long)]
    accounts_file: Option<PathBuf>,
    #[arg(long)]
    claim_ops: bool,
}

#[derive(Default, Deserialize)]
struct Catalog {
    services: BTreeMap<String, Service>,
    #[serde(default)]
    hosts: BTreeMap<String, Host>,
    #[serde(default)]
    users: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    default_account: Option<String>,
    accounts: BTreeMap<String, Enrollment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Enrollment {
    label: Option<String>,
    /// Optional runtime deployment evidence in the rendered inventory.
    #[serde(default)]
    deployments: Vec<Deployment>,
}

#[derive(Deserialize)]
struct Deployment {
    host: String,
    user: String,
}

#[derive(Deserialize)]
struct Host {
    #[serde(default)]
    accounts: BTreeMap<String, String>,
}

impl Catalog {
    fn selected(&self, service: &str, host: &str, user: &str) -> Option<&str> {
        self.hosts
            .get(host)
            .and_then(|h| h.accounts.get(service))
            .or_else(|| self.users.get(user).and_then(|u| u.get(service)))
            .map(String::as_str)
            .or_else(|| self.services.get(service)?.default_account.as_deref())
    }
}

struct Proxy {
    url: Url,
    key: String,
}

#[derive(Default, Deserialize)]
struct ProxyInventory {
    #[serde(default)]
    hub_sync_configured: bool,
    #[serde(skip)]
    unavailable: bool,
    credentials: Vec<Value>,
}

impl ProxyInventory {
    fn from_response(response: Value) -> Result<Self> {
        serde_json::from_value(response).map_err(|_| anyhow::anyhow!("invalid local proxy credential inventory"))
    }
}

struct Collector {
    http: Client,
    host: String,
    user: String,
    uid: u32,
    home: PathBuf,
    catalog: Catalog,
    meridian: Option<Url>,
    proxy: Option<Proxy>,
}

/// Only configured local services may receive these credentials. In particular,
/// don't use environment-configured HTTP proxies or follow redirects.
fn loopback(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid local provider URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url
                .host_str()
                .is_some_and(|h| h == "localhost" || h.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())),
        "provider URL must be loopback HTTP(S) without credentials, query or fragment"
    );
    Ok(url)
}

fn read_local(path: &Path, owner: Option<u32>) -> Result<Option<String>> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let safe = |m: &fs::Metadata| m.is_file() && m.len() <= MAX_INPUT && owner.is_none_or(|uid| m.uid() == uid && m.mode() & 0o077 == 0);
    ensure!(safe(&meta), "expected a small regular file (private and session-owned for credentials)");
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    ensure!(safe(&opened) && meta.ino() == opened.ino() && meta.dev() == opened.dev(), "local file changed while opening");
    let mut value = String::new();
    file.take(MAX_INPUT + 1).read_to_string(&mut value)?;
    ensure!(value.len() as u64 <= MAX_INPUT, "local file exceeds size limit");
    Ok(Some(value))
}

fn str_field(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn datetime(v: &Value) -> Option<DateTime<Utc>> {
    str_field(v)?.parse().ok()
}

fn model_account(provider: &str, id: &str, label: &str) -> ModelAccount {
    // The remaining ModelAccount fields have contract defaults.
    serde_json::from_value(json!({"id": format!("{provider}:{id}"), "provider_id": provider,
        "source_account_id": id, "display_name": label}))
    .expect("valid account fields")
}

pub(super) async fn run(client: &HubClient, options: Options) -> Result<()> {
    let id = |arg: &str| -> Result<String> {
        let out = Command::new("id").arg(arg).output().context("reading Unix session identity")?;
        ensure!(out.status.success(), "could not read Unix session identity");
        Ok(String::from_utf8(out.stdout)?.trim().into())
    };
    let user = id("-un")?;
    let uid = id("-u")?.parse()?;
    ensure!(uid != 0, "run collect as the credential-owning user, not root");
    ensure!(options.user.is_none_or(|u| u == user), "--user must be the invoking Unix user");
    let local_host = fs::read_to_string("/proc/sys/kernel/hostname")?.trim().to_owned();
    let host = options.host.unwrap_or_else(|| local_host.clone());
    super::provider_id(&host).map_err(anyhow::Error::msg)?;
    if options.claim_ops {
        ensure!(host == local_host || local_host.split('.').next() == Some(host.as_str()), "claiming operations requires the local host");
    }
    let catalog = if let Some(path) = options.accounts_file {
        serde_json::from_str(&read_local(&path, None)?.context("accounts file not found")?)
            .map_err(|_| anyhow::anyhow!("invalid accounts catalogue JSON"))?
    } else {
        Catalog::default()
    };
    let proxy = if let (Some(raw_url), Some(path)) = (options.proxy_url, options.proxy_management_key_file) {
        let mut url = loopback(&raw_url)?;
        ensure!(matches!(url.path().trim_end_matches('/'), "" | "/v8/management"), "proxy must expose Spillway v8 management");
        url.set_path("/v8/management/");
        let key =
            read_local(&path, Some(uid)).context("reading private proxy management key")?.context("proxy management key file not found")?;
        ensure!(!key.trim().is_empty(), "proxy management key is empty");
        Some(Proxy { url, key: key.trim().into() })
    } else {
        None
    };
    let collector = Collector {
        http: Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().timeout(Duration::from_secs(20)).build()?,
        host,
        user,
        uid,
        home: env::var_os("HOME").context("HOME is required")?.into(),
        catalog,
        proxy,
        meridian: options.meridian_url.as_deref().map(loopback).transpose()?,
    };
    let proxy = collector.proxy_inventory().await.unwrap_or_else(|error| {
        eprintln!("Could not read local proxy inventory; skipping proxy-owned Antigravity fallback: {error}");
        ProxyInventory { unavailable: true, ..ProxyInventory::default() }
    });
    let mut supported = Vec::new();
    for (provider, label, service) in PROVIDERS {
        let Some(batch) = collector.batch(provider, label, service, &proxy).await else {
            continue;
        };
        if !batch.capabilities.is_empty() {
            supported.push(provider.to_owned());
        }
        if !batch.accounts.is_empty() {
            crate::print(client.ingest_provider_observations(provider, &batch).await?);
        }
    }
    if options.claim_ops && !supported.is_empty() {
        let input =
            ClaimOperationsIn { host: collector.host.clone(), unix_user: collector.user.clone(), provider_ids: supported, limit: 10 };
        for op in client.claim_provider_operations(&input).await? {
            let result = collector.execute(client, &op).await;
            let receipt = match result {
                Ok(()) => OperationReceiptIn { status: OperationStatus::Succeeded, receipt: Some(json!({"applied": true})), error: None },
                Err((status, reason)) => OperationReceiptIn { status, receipt: None, error: Some(reason.into()) },
            };
            let reported = client.report_provider_operation_receipt(op.id, &receipt).await?;
            crate::print(json!({"operation_id": reported.id, "status": reported.status}));
        }
    }
    Ok(())
}

impl Collector {
    fn secret(&self, path: &Path) -> Option<String> {
        match read_local(path, Some(self.uid)) {
            Ok(value) => value,
            Err(_) => {
                eprintln!("Skipped unreadable or unsafe local credential file.");
                None
            }
        }
    }

    fn credential_json(&self, path: &Path) -> Value {
        self.secret(path).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    fn local_account(&self, provider: &str, source: &str, label: &str) -> ModelAccount {
        let mut account = model_account(provider, &format!("{}:{}:{source}", self.host, self.user), label);
        account.enrollment_state = EnrollmentState::Deployed;
        account.capabilities.insert(REFRESH);
        account
    }

    async fn local_accounts(&self, provider: &str, selected: Option<&str>) -> Vec<ModelAccount> {
        match provider {
            "claude" => {
                if let Some(url) = &self.meridian {
                    let key = env::var("MERIDIAN_API_KEY").ok();
                    match probe_meridian_quota(&self.http, url.as_str(), key.as_deref()).await {
                        Ok(rows) if !rows.is_empty() => {
                            return rows
                                .into_iter()
                                .map(|mut a| {
                                    a.email = None;
                                    a.capabilities.insert(REFRESH);
                                    a
                                })
                                .collect();
                        }
                        _ => eprintln!("Meridian quota unavailable; checking local Claude credentials."),
                    }
                }
                let runtime = Path::new("/run/claude-code").join(&self.user);
                let user_runtime = Path::new("/run/user").join(self.uid.to_string()).join("claude-code-token");
                let candidates = [runtime, user_runtime, self.home.join(".claude/.credentials.json")];
                let mut failed = None;
                for (i, path) in candidates.iter().enumerate() {
                    let Some(raw) = self.secret(path) else { continue };
                    let data: Value = serde_json::from_str(raw.trim()).unwrap_or_default();
                    let token = str_field(&data["claudeAiOauth"]["accessToken"])
                        .or_else(|| str_field(&data["access_token"]))
                        .or_else(|| (!raw.trim().starts_with('{')).then_some(raw.trim()));
                    let Some(token) = token.filter(|t| !t.is_empty()) else { continue };
                    let mut a = if i < 2
                        && let Some(id) = selected
                    {
                        let mut a = model_account(provider, id, id);
                        a.is_active = true;
                        a
                    } else {
                        self.local_account(provider, "local", "Local Claude")
                    };
                    a.capabilities.insert(REFRESH);
                    let result = probe_claude_oauth_usage(&self.http, token).await;
                    record(&mut a, result);
                    if a.enrollment_state == EnrollmentState::Verified {
                        return vec![a];
                    }
                    failed.get_or_insert(a);
                }
                failed.into_iter().collect()
            }
            "antigravity" => {
                let gemini = env::var_os("GEMINI_HOME").map_or_else(|| self.home.join(".gemini"), PathBuf::from);
                let data = env::var_os("XDG_DATA_HOME").map_or_else(|| self.home.join(".local/share"), PathBuf::from);
                let t3_profile = hex::encode(ring::digest::digest(&ring::digest::SHA256, b"antigravity"));
                let acp_token = "antigravity-acp/acp_token.json";
                let mut paths = vec![
                    data.join("t3code-native/userdata/providers/antigravity").join(t3_profile).join(acp_token),
                    gemini.join(acp_token),
                ];
                if let Some(path) = env::var_os("T3_ANTIGRAVITY_TOKEN_FILE") {
                    paths.insert(0, path.into());
                }
                for path in paths {
                    let raw = self.credential_json(&path);
                    let (Some(client), Some(secret), Some(refresh)) = (
                        str_field(&raw["client_id"]),
                        str_field(&raw["client_secret"]),
                        str_field(&raw["refresh_token"]).or_else(|| str_field(&raw["token"]["refresh_token"])),
                    ) else {
                        continue;
                    };
                    let mut a = self.local_account(provider, "local", "Local Antigravity");
                    let result = probe_antigravity_quota(&self.http, client, secret, refresh, str_field(&raw["project_id"])).await.map(
                        |(fingerprint, windows)| {
                            a.credential_fingerprint = fingerprint;
                            windows
                        },
                    );
                    record(&mut a, result);
                    return vec![a];
                }
                Vec::new()
            }
            "opencode" => {
                let data = env::var_os("XDG_DATA_HOME").map_or_else(|| self.home.join(".local/share"), PathBuf::from);
                let code = self.credential_json(&data.join("opencode/auth.json"));
                let codex_dir = env::var_os("CODEX_HOME").map_or_else(|| self.home.join(".codex"), PathBuf::from);
                let codex = self.credential_json(&codex_dir.join("auth.json"));
                let mut accounts = Vec::new();
                if let Some(key) = str_field(&code["opencode-go"]["key"]) {
                    let mut a = self.local_account(provider, "go", "OpenCode Go");
                    record(&mut a, probe_opencode_go_usage(&self.http, key).await);
                    accounts.push(a);
                }
                let identities = [
                    (
                        "openai",
                        "OpenCode ChatGPT",
                        str_field(&code["openai"]["access"]).or_else(|| str_field(&code["openai"]["accessToken"])),
                        str_field(&code["openai"]["accountId"]).or_else(|| str_field(&code["openai"]["account_id"])),
                    ),
                    ("codex", "Codex ChatGPT", str_field(&codex["tokens"]["access_token"]), str_field(&codex["tokens"]["account_id"])),
                ];
                for (source, label, token, id) in identities {
                    let Some(token) = token else { continue };
                    let mut a = self.local_account(provider, source, label);
                    let result = probe_chatgpt_wham_usage(&self.http, token, id).await.map(|(plan, windows, grant)| {
                        a.plan = plan;
                        a.reset_grant = grant;
                        a.credential_fingerprint = id.map(|id| hex::encode(ring::digest::digest(&ring::digest::SHA256, id.as_bytes())));
                        windows
                    });
                    record(&mut a, result);
                    accounts.push(a);
                }
                accounts
            }
            _ => Vec::new(),
        }
    }

    async fn batch(&self, provider: &str, label: &str, service: &str, proxy: &ProxyInventory) -> Option<ObservationBatch> {
        if matches!(provider, "antigravity" | "opencode") && (proxy.hub_sync_configured || proxy.unavailable) {
            return None;
        }
        let now = Utc::now();
        let selected = self.catalog.selected(service, &self.host, &self.user);
        let proxy_accounts = self.proxy_accounts(provider, &proxy.credentials, now);
        // A reachable proxy inventory already represents the pooled accounts;
        // the catalogue and local files can describe the same accounts under other IDs.
        let proxy_owns_accounts = matches!(provider, "antigravity" | "opencode") && !proxy_accounts.is_empty();
        let mut accounts = BTreeMap::new();
        if !proxy_owns_accounts && let Some(entry) = self.catalog.services.get(service) {
            for (id, entry) in &entry.accounts {
                let mut a = model_account(provider, id, entry.label.as_deref().unwrap_or(id));
                let deployed = entry.deployments.iter().any(|d| d.host == self.host && d.user == self.user);
                a.is_preferred = Some(id.as_str()) == selected;
                a.enrollment_state = if a.is_preferred {
                    EnrollmentState::Selected
                } else if deployed {
                    EnrollmentState::Deployed
                } else {
                    EnrollmentState::Catalogued
                };
                accounts.insert(a.id.clone(), a);
            }
        }
        let local = if proxy_owns_accounts { Vec::new() } else { self.local_accounts(provider, selected).await };
        for mut a in local.into_iter().chain(proxy_accounts) {
            if let Some(catalogued) = accounts.get(&a.id) {
                a.is_preferred |= catalogued.is_preferred;
                a.display_name.clone_from(&catalogued.display_name);
            }
            accounts.insert(a.id.clone(), a);
        }
        let accounts: Vec<_> = accounts.into_values().collect();
        let capabilities = accounts.iter().flat_map(|a| a.capabilities.iter().copied()).collect();
        let health = if accounts
            .iter()
            .any(|a| a.enabled && a.last_error.as_ref().map_or(a.status_message.is_some(), AccountError::is_notification_worthy))
        {
            ProviderHealth::Degraded
        } else if accounts.iter().any(|a| a.enrollment_state == EnrollmentState::Verified) {
            ProviderHealth::Healthy
        } else {
            ProviderHealth::Unobserved
        };
        Some(ObservationBatch {
            provider_id: provider.into(),
            provider_label: label.into(),
            provider_family: provider.into(),
            source_host: self.host.clone(),
            source_user: self.user.clone(),
            source_publisher: Some("fleetix-cli-collector".into()),
            collected_at: now,
            account_inventory_complete: false,
            health,
            status_message: None,
            router_id: None,
            capabilities,
            evidence_kinds: [EvidenceKind::AccountInventory, EvidenceKind::QuotaWindows, EvidenceKind::ResetGrants].into(),
            accounts,
            quota_samples: Vec::new(),
            reset_grants: Vec::new(),
            key_groups: Vec::new(),
            models: Vec::new(),
            routing_status: None,
            proxy_logs: Vec::new(),
        })
    }

    async fn proxy_request(&self, method: Method, path: &str, payload: Option<Value>) -> Result<Value> {
        let proxy = self.proxy.as_ref().context("local proxy is not configured")?;
        let mut request = self.http.request(method, proxy.url.join(path)?).bearer_auth(&proxy.key);
        if let Some(body) = payload {
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(|_| anyhow::anyhow!("local proxy request failed"))?;
        ensure!(response.status().is_success(), "local proxy rejected the request");
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| anyhow::anyhow!("local proxy response failed"))? {
            ensure!(body.len() + chunk.len() <= MAX_INPUT as usize, "local proxy response is too large");
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).map_err(|_| anyhow::anyhow!("invalid local proxy response"))
    }

    async fn proxy_inventory(&self) -> Result<ProxyInventory> {
        if self.proxy.is_none() {
            return Ok(ProxyInventory::default());
        }
        ProxyInventory::from_response(self.proxy_request(Method::GET, "usage/windows", None).await?)
    }

    fn proxy_id(&self, provider: &str, row: &Value) -> Option<String> {
        proxy_family(provider, row)?;
        let id = str_field(&row["id"])?;
        Some(format!("{provider}:{}:{}:{id}", self.host, self.user))
    }

    fn proxy_accounts(&self, provider: &str, rows: &[Value], now: DateTime<Utc>) -> Vec<ModelAccount> {
        rows.iter()
            .filter_map(|row| {
                let id = self.proxy_id(provider, row)?;
                let source_id = id.strip_prefix(provider)?.strip_prefix(':')?;
                let mut a = model_account(provider, source_id, str_field(&row["label"]).unwrap_or("Local proxy account"));
                a.masked_email = str_field(&row["email"]).map(mask_email);
                a.plan = str_field(&row["plan"]["label"]).map(str::to_owned);
                a.enabled = row["disabled"].as_bool() != Some(true);
                a.priority = row["priority"].as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(0);
                a.weight = row["weight"].as_u64().and_then(|n| i32::try_from(n).ok()).map_or(1, |n| n as u32);
                a.is_preferred = row["preferred"].as_bool() == Some(true);
                a.is_next_pick = row["next_pick"].as_bool() == Some(true);
                a.cooldown_until = datetime(&row["cooldown_until"]);
                a.last_observed_at = datetime(&row["observed_at"]);
                a.enrollment_state = if a.last_observed_at.is_some() { EnrollmentState::Verified } else { EnrollmentState::Deployed };
                a.last_error = serde_json::from_value::<AccountError>(row["last_error"].clone()).ok().map(AccountError::sanitized);
                a.last_error_observed = a.last_error.is_some() || row.get("last_error").is_some_and(Value::is_null);
                let legacy_error = a.last_error.is_none() && matches!(row["status"].as_str(), Some("error" | "unavailable"));
                if legacy_error {
                    a.enrollment_state = EnrollmentState::Error;
                }
                a.status = AccountStatus::resolve_with_error(
                    a.enabled,
                    a.cooldown_until,
                    now,
                    a.is_next_pick,
                    legacy_error,
                    a.last_error.as_ref(),
                );
                a.status_message = a
                    .last_error
                    .as_ref()
                    .map(AccountError::summary)
                    .or_else(|| legacy_error.then(|| "Local proxy reports an account error".into()));
                if str_field(&row["auth_index"]).is_some() && !row["api_key"].is_object() {
                    a.capabilities.extend([REFRESH, OVERRIDE]);
                }
                a.windows = row["windows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|w| {
                        let key = str_field(&w["key"])?;
                        let used = w["used_percent"].as_f64().map(|n| n.clamp(0.0, 100.0));
                        let stale = w["stale"].as_bool() == Some(true);
                        let minutes = w["duration_seconds"].as_u64().and_then(|n| u32::try_from(n / 60).ok()).filter(|n| *n > 0);
                        Some(QuotaWindow {
                            window_key: key.into(),
                            label: str_field(&w["label"]).unwrap_or(key).into(),
                            kind: match (w["scoped"].as_bool(), minutes) {
                                (Some(true), _) => QuotaWindowKind::ModelScoped,
                                (_, Some(300)) => QuotaWindowKind::Session,
                                (_, Some(1440)) => QuotaWindowKind::Daily,
                                (_, Some(10080)) => QuotaWindowKind::Weekly,
                                _ => QuotaWindowKind::Custom,
                            },
                            unit: QuotaUnit::Percent,
                            used_percent: used,
                            remaining_percent: used.map(|n| 100.0 - n),
                            used_amount: None,
                            limit_amount: None,
                            currency: None,
                            resets_at: datetime(&w["resets_at"]),
                            window_duration_mins: minutes,
                            cadence: match w["reset_cadence"].as_str() {
                                Some("on_use") => ResetCadence::RollingOnUse,
                                Some("fixed") => ResetCadence::Fixed,
                                _ => ResetCadence::Unknown,
                            },
                            scope_model: None,
                            status: QuotaStatus::Unknown,
                            observation_state: match (used, stale) {
                                (Some(_), true) => QuotaObservationState::StaleLastGood,
                                (Some(_), false) => QuotaObservationState::Measured,
                                (None, _) => QuotaObservationState::Unavailable,
                            },
                            observed_at: datetime(&w["observed_at"]).or(a.last_observed_at),
                            stale,
                            error_reason: None,
                        })
                    })
                    .collect();
                Some(a)
            })
            .collect()
    }

    async fn execute(&self, client: &HubClient, op: &ProviderOperation) -> Result<(), (OperationStatus, &'static str)> {
        let failed = |message| (OperationStatus::Failed, message);
        let ambiguous = (OperationStatus::Ambiguous, "Local proxy write outcome is unknown");
        let provider = op.provider_id.as_str();
        let (_, label, service) =
            PROVIDERS.iter().copied().find(|(p, _, _)| *p == provider).ok_or_else(|| failed("Provider is not local"))?;
        if op.target_host.as_deref().is_some_and(|h| h != self.host) || op.target_user.as_deref().is_some_and(|u| u != self.user) {
            return Err(failed("Operation targets a different session"));
        }
        let proxy = self.proxy_inventory().await.map_err(|_| failed("Could not read local proxy inventory"))?;
        if matches!(provider, "antigravity" | "opencode") && proxy.hub_sync_configured {
            return Err(failed("Provider operations are owned by the native proxy"));
        }
        match &op.command {
            ProviderCommand::Quota(QuotaCommand::RefreshQuota { account_id }) => {
                let target = account_id.as_ref().or(op.target_account_id.as_ref());
                for row in &proxy.credentials {
                    if let Some(id) = self.proxy_id(provider, row)
                        && target.is_none_or(|target| target == &id)
                        && row["disabled"].as_bool() != Some(true)
                        && !row["api_key"].is_object()
                        && let Some(index) = str_field(&row["auth_index"])
                    {
                        let result = self
                            .proxy_request(Method::POST, "usage/refresh", Some(json!({"auth_index": index})))
                            .await
                            .map_err(|_| ambiguous)?;
                        if result["errors"].as_object().is_some_and(|errors| !errors.is_empty()) {
                            return Err(failed("Local proxy reported a refresh failure"));
                        }
                    }
                }
                let proxy = self.proxy_inventory().await.map_err(|_| failed("Could not read refreshed proxy inventory"))?;
                let batch =
                    self.batch(provider, label, service, &proxy).await.ok_or_else(|| failed("Provider is owned by the native proxy"))?;
                if !batch.accounts.iter().any(|a| {
                    target.is_none_or(|id| id == &a.id)
                        && a.capabilities.contains(&REFRESH)
                        && a.enrollment_state == EnrollmentState::Verified
                        && a.status_message.is_none()
                }) {
                    return Err(failed("No matching local account was verified"));
                }
                client
                    .ingest_provider_observations(provider, &batch)
                    .await
                    .map_err(|_| failed("Hub did not accept refreshed observations"))?;
            }
            ProviderCommand::Routing(RoutingCommand::SetPoolOverride { account_id: Some(target) }) => {
                if op.target_host.as_deref() != Some(&self.host) || op.target_user.as_deref() != Some(&self.user) {
                    return Err(failed("Local proxy switching requires an explicit host and user"));
                }
                let (family, index) = proxy
                    .credentials
                    .iter()
                    .find_map(|row| {
                        (self.proxy_id(provider, row)?.as_str() == target
                            && row["disabled"].as_bool() != Some(true)
                            && !row["api_key"].is_object())
                        .then_some((proxy_family(provider, row)?, str_field(&row["auth_index"])?))
                    })
                    .ok_or_else(|| failed("Account is not switchable on the local proxy"))?;
                self.proxy_request(Method::PUT, "routing/preferred-auth", Some(json!({"provider": family, "auth_index": index})))
                    .await
                    .map_err(|_| ambiguous)?;
            }
            _ => return Err(failed("This operation has no supported local implementation")),
        }
        Ok(())
    }
}

fn proxy_family<'a>(provider: &str, row: &'a Value) -> Option<&'a str> {
    let family = str_field(&row["provider"]).or_else(|| str_field(&row["type"]))?;
    match (provider, family) {
        ("claude", "claude") | ("antigravity", "antigravity") | ("opencode", "opencode" | "openai" | "codex") => Some(family),
        _ => None,
    }
}

fn record(account: &mut ModelAccount, result: Result<Vec<QuotaWindow>, String>) {
    match result {
        Ok(windows) => {
            if account.reset_grant.is_some() || windows.iter().any(|w| w.observation_state == QuotaObservationState::Measured) {
                account.last_observed_at = Some(Utc::now());
                account.enrollment_state = EnrollmentState::Verified;
            }
            account.windows = windows;
        }
        Err(_) => {
            account.status = AccountStatus::Error;
            account.status_message = Some("Local quota probe unavailable".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn antigravity_uses_one_proxy_publisher_and_canonical_ids() {
        let collector = Collector {
            http: Client::new(),
            host: "host-a".into(),
            user: "operator".into(),
            uid: 1000,
            home: PathBuf::new(),
            catalog: serde_json::from_value(json!({"services":{"google":{"accounts":{"primary":{}}}}})).unwrap(),
            meridian: None,
            proxy: None,
        };
        let mut credentials: Vec<_> = (1..=6).map(|i| json!({"provider":"antigravity","id":format!("google-{i}")})).collect();
        credentials[0]["next_pick"] = json!(true);
        credentials[0]["last_error"] = json!({
            "kind":"verification_required", "source":"oauth_refresh", "occurred_at":"2026-11-22T10:00:00Z",
            "message":"Verify your account", "consecutive_count":1, "action_hint":"Sign in to Antigravity"
        });
        let native = ProxyInventory::from_response(json!({"credentials":credentials,"hub_sync_configured":true})).unwrap();
        assert!(collector.batch("antigravity", "Antigravity", "google", &native).await.is_none());
        assert!(collector.batch("opencode", "OpenCode", "openai", &native).await.is_none());
        let unavailable = ProxyInventory { unavailable: true, ..ProxyInventory::default() };
        assert!(collector.batch("antigravity", "Antigravity", "google", &unavailable).await.is_none());
        assert!(collector.batch("opencode", "OpenCode", "openai", &unavailable).await.is_none());
        let external = ProxyInventory::from_response(json!({"credentials":credentials})).unwrap();
        let accounts = collector.batch("antigravity", "Antigravity", "google", &external).await.unwrap().accounts;
        assert_eq!(accounts.len(), 6);
        assert_eq!(accounts[0].id, "antigravity:host-a:operator:google-1");
        assert_eq!(accounts[0].status, AccountStatus::Error);
        assert_eq!(accounts[0].status_message.as_deref(), Some("Verification required: Verify your account"));
        let error = accounts[0].last_error.as_ref().unwrap();
        assert_eq!(error.source, AccountErrorSource::OAuthRefresh);
        assert_eq!(error.action_hint.as_deref(), Some("Sign in to Antigravity"));
        assert!(accounts[1].last_error.is_none());
        assert!(!accounts[1].last_error_observed);
        let mut transient = external;
        transient.credentials[1]["last_error"] = Value::Null;
        transient.credentials[0]["last_error"]["kind"] = json!("upstream_unavailable");
        let batch = collector.batch("antigravity", "Antigravity", "google", &transient).await.unwrap();
        assert!(batch.accounts[1].last_error_observed);
        assert_ne!(batch.health, ProviderHealth::Degraded, "one transient error must not trigger the fallback alert");
    }

    #[test]
    fn catalogue_selects_host_then_user_then_fleet() {
        let catalog: Catalog = serde_json::from_value(json!({
            "services": {"google": {"defaultAccount": "fleet", "accounts": {
                "fleet": {"sourceHost": "a", "secretName": "not-a-token"},
                "user": {},
                "host": {}
            }}},
            "users": {"alice": {"google": "user"}},
            "hosts": {"atlas": {"accounts": {"google": "host"}}}
        }))
        .unwrap();
        assert_eq!(catalog.selected("google", "atlas", "alice"), Some("host"));
        assert_eq!(catalog.selected("google", "nomad", "alice"), Some("user"));
        assert_eq!(catalog.selected("google", "nomad", "bob"), Some("fleet"));
        assert_eq!(catalog.services["google"].accounts.len(), 3);
    }

    #[test]
    fn credentials_cannot_be_forwarded_to_remote_or_deceptive_origins() {
        for url in ["http://localhost.evil.invalid", "https://user@127.0.0.1", "http://127.0.0.1?token=x", "https://192.0.2.1"] {
            assert!(loopback(url).is_err(), "{url}");
        }
        assert!(loopback("http://127.0.0.1:8080").is_ok());
        assert!(loopback("http://[::1]:8080").is_ok());
    }
}
