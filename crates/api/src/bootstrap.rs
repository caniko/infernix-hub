//! Configuration from the environment, adapter selection and registry seeding.
//!
//! Each integration is independently `fixture` or `live`. Secrets are read
//! from `*_TOKEN_FILE` paths (systemd credentials / agenix), never from argv,
//! and never leave the server.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use contracts::{AccountCapabilities, AdapterMode, IntegrationStatus, Verification};
use domain::ports;
use domain::{Hub, HubConfig};
use integrations::fixture::{FixtureAssistant, FixtureComfy, FixtureGpuPeer, FixturePaperclip, FixturePublisher, FixtureWindmill};
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;

#[derive(Debug, Clone)]
pub struct Settings {
    pub database_url: String,
    pub bind: String,
    pub public_url: String,
    pub asset_dir: PathBuf,
    pub registry_dir: PathBuf,
    pub seed_file: Option<PathBuf>,
    /// Fleet snapshot written by `canix fleet status --write`; absent when the
    /// hub has no fleet configured.
    pub fleet_status_file: Option<PathBuf>,
    pub company_id: Option<String>,
    pub paperclip: AdapterMode,
    pub windmill: AdapterMode,
    pub comfy: AdapterMode,
    pub publisher: AdapterMode,
    pub assistant: AdapterMode,
    /// Pack enablement and per-pack JSON config (id -> object).
    pub packs_config: Option<PathBuf>,
    /// Do not start the outbox worker with `serve`.
    pub no_worker: bool,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn mode(key: &str) -> anyhow::Result<AdapterMode> {
    let v = env(key).unwrap_or_else(|| "fixture".into());
    AdapterMode::parse(&v).with_context(|| format!("{key} must be fixture|live|disabled, got {v}"))
}

fn secret(key: &str) -> anyhow::Result<String> {
    let path = env(&format!("{key}_FILE")).with_context(|| format!("{key}_FILE is required for live mode"))?;
    Ok(std::fs::read_to_string(&path).with_context(|| format!("reading {key}_FILE"))?.trim().to_string())
}

impl Settings {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind = env("HUB_BIND").unwrap_or_else(|| "127.0.0.1:8470".into());
        Ok(Self {
            database_url: env("DATABASE_URL").context("DATABASE_URL is required")?,
            public_url: env("HUB_PUBLIC_URL").unwrap_or_else(|| format!("http://{bind}")),
            bind,
            asset_dir: env("HUB_ASSET_DIR").map(PathBuf::from).unwrap_or_else(|| "var/assets".into()),
            registry_dir: env("HUB_REGISTRY_DIR").map(PathBuf::from).unwrap_or_else(|| "registry".into()),
            seed_file: env("HUB_SEED_FILE").map(PathBuf::from),
            fleet_status_file: env("HUB_FLEET_STATUS_FILE").map(PathBuf::from),
            company_id: env("HUB_COMPANY_ID"),
            paperclip: mode("HUB_PAPERCLIP")?,
            windmill: mode("HUB_WINDMILL")?,
            comfy: mode("HUB_COMFY")?,
            publisher: mode("HUB_PUBLISHER")?,
            assistant: mode("HUB_ASSISTANT")?,
            packs_config: env("HUB_PACKS_CONFIG").map(PathBuf::from),
            no_worker: matches!(env("HUB_NO_WORKER").as_deref(), Some("1" | "true")),
        })
    }

    /// Everything simulated; for `dev` and tests.
    pub fn fixture(database_url: &str, bind: &str) -> Self {
        Self {
            database_url: database_url.into(),
            bind: bind.into(),
            public_url: format!("http://{bind}"),
            asset_dir: "var/assets".into(),
            registry_dir: "registry".into(),
            seed_file: Some("registry/hub.fixture.json".into()),
            fleet_status_file: None,
            company_id: None,
            paperclip: AdapterMode::Fixture,
            windmill: AdapterMode::Fixture,
            comfy: AdapterMode::Fixture,
            publisher: AdapterMode::Fixture,
            assistant: AdapterMode::Fixture,
            packs_config: None,
            no_worker: false,
        }
    }
}

/// Handles to in-process fixtures (tests drive them directly).
#[derive(Clone, Default)]
pub struct Fixtures {
    pub paperclip: Option<Arc<FixturePaperclip>>,
    pub comfy: Option<Arc<FixtureComfy>>,
    pub windmill: Option<FixtureWindmill>,
    pub publisher: Option<Arc<FixturePublisher>>,
    pub gpu_peer: Option<Arc<FixtureGpuPeer>>,
}

struct Disabled(&'static str);

fn disabled<T>(what: &'static str) -> ports::PortResult<T> {
    Err(ports::PortError::Unsupported(format!("{what} integration is disabled")))
}

#[async_trait::async_trait]
impl ports::Publisher for Disabled {
    async fn submit(&self, _: &ports::PublishRequest) -> ports::PortResult<ports::ProviderPost> {
        disabled(self.0)
    }
    async fn lookup(&self, _: &str, _: &str) -> ports::PortResult<Option<ports::ProviderPost>> {
        disabled(self.0)
    }
    async fn reply(&self, _: &ports::ReplyRequest) -> ports::PortResult<ports::ProviderPost> {
        disabled(self.0)
    }
    async fn lookup_reply(&self, _: &str, _: &str) -> ports::PortResult<Option<ports::ProviderPost>> {
        disabled(self.0)
    }
}

fn status(name: &str, mode: AdapterMode, live: Verification, detail: &str) -> IntegrationStatus {
    let verification = match mode {
        AdapterMode::Fixture => Verification::FixtureTested,
        AdapterMode::Live => live,
        AdapterMode::Disabled => Verification::Built,
    };
    let detail = match mode {
        AdapterMode::Fixture => "simulated in-process; proves hub logic only".to_string(),
        _ => detail.to_string(),
    };
    IntegrationStatus { name: name.into(), mode, verification, detail }
}

/// Fixture approvals can never authorize a real post, whatever `HUB_PUBLISHER`
/// says. Only a channel backed by the in-process fixture adapter (or the
/// disabled publisher) is simulated; every other transport — the built-in live
/// adapter, each `n8n:<platform>` channel and any pack channel — publishes for
/// real and therefore needs live Paperclip approvals.
pub(crate) fn require_live_approvals(
    paperclip: AdapterMode,
    publisher: AdapterMode,
    builtin: &Arc<dyn ports::Publisher>,
    channels: &BTreeMap<String, Arc<dyn ports::Publisher>>,
) -> anyhow::Result<()> {
    if paperclip == AdapterMode::Live {
        return Ok(());
    }
    let simulated = |channel: &Arc<dyn ports::Publisher>| publisher != AdapterMode::Live && Arc::ptr_eq(channel, builtin);
    for (transport, channel) in channels {
        if !simulated(channel) {
            bail!(
                "publishing channel {transport:?} posts for real, but HUB_PAPERCLIP={}: \
                 fixture approvals cannot authorize real publishing (set HUB_PAPERCLIP=live or remove the channel)",
                paperclip
            );
        }
    }
    Ok(())
}

pub async fn build(s: &Settings) -> anyhow::Result<(AppState, Fixtures)> {
    // Only the publisher has a real disabled mode; the others would silently
    // fall back to a simulation that the status page reports as disabled.
    for (name, mode) in
        [("HUB_PAPERCLIP", s.paperclip), ("HUB_WINDMILL", s.windmill), ("HUB_COMFY", s.comfy), ("HUB_ASSISTANT", s.assistant)]
    {
        if mode == AdapterMode::Disabled {
            bail!("{name}=disabled is not supported; use fixture or live");
        }
    }
    // Simulated approvals must never authorize a real post. This early check
    // covers the built-in adapter before the database is touched; the
    // per-channel guard below also covers n8n and pack channels.
    if s.publisher == AdapterMode::Live && s.paperclip != AdapterMode::Live {
        bail!("HUB_PUBLISHER=live requires HUB_PAPERCLIP=live (fixture approvals cannot authorize real publishing)");
    }
    if s.windmill == AdapterMode::Fixture && s.comfy != AdapterMode::Fixture {
        bail!("HUB_WINDMILL=fixture runs the procedure in-process and needs HUB_COMFY=fixture");
    }
    let db = domain::sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect(&s.database_url)
        .await
        .context("connecting to PostgreSQL")?;
    // Under harbor-db the runtime role cannot run DDL: migrations are a
    // separate unit and the server refuses to start against a stale schema.
    if env("HUB_MIGRATE_ON_START").as_deref() == Some("0") {
        let pending = domain::pending_migrations(&db).await?;
        if !pending.is_empty() {
            bail!("database has pending migrations {pending:?}; run `fleetix-hubd migrate` first");
        }
    } else {
        domain::MIGRATOR.run(&db).await.context("running migrations")?;
    }
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let mut fx = Fixtures::default();
    let mut windmill_client: Option<integrations::live::WindmillClient> = None;

    let paperclip: Arc<dyn ports::Paperclip> = match s.paperclip {
        AdapterMode::Live => Arc::new(integrations::live::PaperclipClient::new(
            http.clone(),
            integrations::live::PaperclipConfig::new(
                env("PAPERCLIP_URL").unwrap_or_else(|| "http://127.0.0.1:3117".into()),
                secret("PAPERCLIP_TOKEN")?,
            ),
        )),
        _ => {
            let f = Arc::new(FixturePaperclip::default());
            fx.paperclip = Some(f.clone());
            f
        }
    };
    let comfy: Arc<dyn ports::Comfy> = match s.comfy {
        AdapterMode::Live => Arc::new(integrations::live::ComfyClient::new(http.clone())),
        _ => {
            let f = Arc::new(FixtureComfy::default());
            fx.comfy = Some(f.clone());
            f
        }
    };
    // The GPU co-tenant check follows the engine: a live ComfyUI shares a
    // real GPU with llama-swap; a fixture engine gets a fixture peer.
    let gpu_peer: Arc<dyn ports::GpuPeer> = match s.comfy {
        AdapterMode::Live => Arc::new(integrations::gpu_peer::LlamaSwapPeer::new(http.clone())),
        _ => {
            let f = Arc::new(FixtureGpuPeer::default());
            fx.gpu_peer = Some(f.clone());
            f
        }
    };
    let windmill: Arc<dyn ports::Windmill> = match s.windmill {
        AdapterMode::Live => {
            let client = integrations::live::WindmillClient::new(
                http.clone(),
                integrations::live::WindmillConfig::new(
                    env("WINDMILL_URL").unwrap_or_else(|| "http://127.0.0.1:8040".into()),
                    env("WINDMILL_WORKSPACE").unwrap_or_else(|| "studio".into()),
                    secret("WINDMILL_TOKEN")?,
                ),
            );
            windmill_client = Some(client.clone());
            Arc::new(client)
        }
        _ => {
            let f = FixtureWindmill::new(fx.comfy.clone().expect("checked above"));
            fx.windmill = Some(f.clone());
            Arc::new(f)
        }
    };
    let publisher: Arc<dyn ports::Publisher> = match s.publisher {
        AdapterMode::Live => Arc::new(integrations::live::PostizClient::new(
            http.clone(),
            integrations::live::PostizConfig::new(
                env("POSTIZ_URL").context("POSTIZ_URL is required for a live publisher")?,
                secret("POSTIZ_TOKEN")?,
            ),
        )),
        AdapterMode::Disabled => Arc::new(Disabled("publisher")),
        AdapterMode::Fixture => {
            let f = Arc::new(FixturePublisher::default());
            fx.publisher = Some(f.clone());
            f
        }
    };
    let assistant: Arc<dyn ports::Assistant> = match s.assistant {
        AdapterMode::Live => Arc::new(integrations::assistant::OpenAiAssistant::new(
            &env("LLM_URL").unwrap_or_else(|| "http://127.0.0.1:8013/v1".into()),
            &env("LLM_MODEL").unwrap_or_else(|| "qwen3.8-27b".into()),
        )),
        _ => Arc::new(FixtureAssistant),
    };

    let seed: Option<Seed> = match &s.seed_file {
        Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?)?),
        None => None,
    };
    let company_id = s
        .company_id
        .clone()
        .or_else(|| seed.as_ref().and_then(|s| s.company_id.clone()))
        .context("HUB_COMPANY_ID (the Paperclip company id) is required")?;

    let config = HubConfig {
        public_url: s.public_url.trim_end_matches('/').to_string(),
        company_id,
        integrations: vec![
            status(
                "paperclip",
                s.paperclip,
                Verification::ContractVerified,
                "board API 0.3.1; OpenAPI-checked, no authenticated call made yet",
            ),
            status("windmill", s.windmill, Verification::ContractVerified, "CE 1.839.0; OpenAPI-checked, procedures not deployed"),
            status("comfyui", s.comfy, Verification::ContractVerified, "0.37.0; checked against server source"),
            status("publisher", s.publisher, Verification::Built, "Postiz adapter built from public docs; Postiz not deployed"),
            status("assistant", s.assistant, Verification::ContractVerified, "llama-swap OpenAI-compatible endpoint"),
        ],
        dispatch_early_secs: 30,
        dispatch_window_secs: 15 * 60,
    };
    // Publishing channels are addressed by `accounts.transport`. The built-in
    // adapter answers under its own real name only; a `fixture` deployment
    // aliases the seeded account transports to the in-process fixture adapter
    // so core dispatch keeps working. In live mode an unregistered transport
    // stays `Dead` for an operator; it never falls back to another provider.
    let mut channels: BTreeMap<String, Arc<dyn ports::Publisher>> = BTreeMap::new();
    let builtin_transport = match s.publisher {
        AdapterMode::Live => "postiz",
        AdapterMode::Fixture => "fixture",
        AdapterMode::Disabled => "disabled",
    };
    channels.insert(builtin_transport.to_string(), publisher.clone());
    if s.publisher == AdapterMode::Fixture
        && let Some(seed) = &seed
    {
        for account in &seed.accounts {
            channels.insert(account.transport.clone(), publisher.clone());
        }
    }
    // n8n contract v1: one live channel per platform workflow,
    // `n8n:<platform>`. It replaces a fixture alias of the same name.
    if let Some(platforms) = env("N8N_PUBLISH_PLATFORMS") {
        let url = env("N8N_PUBLISH_URL").context("N8N_PUBLISH_URL is required with N8N_PUBLISH_PLATFORMS")?;
        let token = secret("N8N_WEBHOOK_TOKEN")?;
        for platform in platforms.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let cfg = integrations::live::N8nPublisherConfig::new(url.clone(), token.clone(), platform);
            channels.insert(format!("n8n:{platform}"), Arc::new(integrations::live::N8nPublisher::new(http.clone(), cfg)));
        }
    }
    require_live_approvals(s.paperclip, s.publisher, &publisher, &channels).context("publishing channels")?;
    // Built-in pipelines are the upstream registry's pipeline workflows: one
    // generic Windmill script per `pipeline_id` (or its in-process stand-in).
    // Packs register their own pipelines and workflow rows.
    let mut pipelines: BTreeMap<String, Arc<dyn ports::Pipeline>> = BTreeMap::new();
    for spec in read_registry(&s.registry_dir.join("pipeline"))? {
        let Some(id) = spec.pipeline_id.clone() else { continue };
        let pipeline: Arc<dyn ports::Pipeline> = match &windmill_client {
            Some(client) => Arc::new(integrations::live::WindmillPipeline::new(client.clone(), spec.windmill_path.clone())),
            None => Arc::new(integrations::fixture::FixturePipeline::new(
                fx.windmill.clone().context("fixture windmill is required for a fixture pipeline")?,
            )),
        };
        pipelines.insert(id, pipeline);
    }
    let hub = Hub {
        db,
        config: Arc::new(config),
        paperclip,
        windmill,
        comfy,
        publisher,
        assistant,
        gpu_peer,
        assets: Arc::new(integrations::store::LocalAssetStore::new(&s.asset_dir)),
        channels: Arc::new(channels),
        pipelines: Arc::new(pipelines),
        topics: Arc::new(BTreeMap::new()),
    };
    if let Some(seed) = &seed {
        apply_seed(&hub, seed, &hub.config.company_id).await?;
    }
    load_registry(&hub, &s.registry_dir.join("comfyui")).await?;
    load_registry(&hub, &s.registry_dir.join("pipeline")).await?;
    Ok((AppState { hub, fixture_paperclip: fx.paperclip.clone(), fleet_status_file: s.fleet_status_file.clone() }, fx))
}

#[derive(Debug, Deserialize)]
pub struct Seed {
    /// Only fixture seeds carry a company id; live runs use HUB_COMPANY_ID.
    pub company_id: Option<String>,
    #[serde(default)]
    pub engines: Vec<SeedEngine>,
    #[serde(default)]
    pub personas: Vec<SeedPersona>,
    #[serde(default)]
    pub accounts: Vec<SeedAccount>,
}

#[derive(Debug, Deserialize)]
pub struct SeedEngine {
    pub id: String,
    pub base_url: String,
    #[serde(default)]
    pub capabilities: Value,
}

#[derive(Debug, Deserialize)]
pub struct SeedPersona {
    pub name: String,
    pub voice: String,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub avoid: Vec<String>,
    #[serde(default)]
    pub world: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SeedAccount {
    pub id: String,
    pub platform: String,
    pub handle: String,
    pub transport: String,
    pub persona: Option<String>,
    pub capabilities: AccountCapabilities,
}

/// Idempotent seed: engines, personas (with accepted canon r1) and accounts.
pub async fn apply_seed(hub: &Hub, seed: &Seed, company_id: &str) -> anyhow::Result<()> {
    for e in &seed.engines {
        domain::production::register_engine(hub, &e.id, &e.base_url, e.capabilities.clone()).await?;
    }
    for p in &seed.personas {
        let exists: Option<uuid::Uuid> =
            domain::sqlx::query_scalar("select id from personas where name = $1").bind(&p.name).fetch_optional(&hub.db).await?;
        if exists.is_some() {
            continue;
        }
        let id = uuid::Uuid::new_v4();
        domain::sqlx::query("insert into personas (id, company_id, name) values ($1, $2, $3)")
            .bind(id)
            .bind(company_id)
            .bind(&p.name)
            .execute(&hub.db)
            .await?;
        let body = contracts::CanonBody { voice: p.voice.clone(), rules: p.rules.clone(), avoid: p.avoid.clone(), world: p.world.clone() };
        domain::sqlx::query(
            "insert into canon_revisions (id, persona_id, revision, body, state, source, rationale, proposed_by, decided_by, decided_at)
             values ($1, $2, 1, $3, 'accepted', 'operator', 'seed', 'seed', 'seed', now())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(id)
        .bind(serde_json::to_value(&body)?)
        .execute(&hub.db)
        .await?;
    }
    for a in &seed.accounts {
        let persona_id = match &a.persona {
            Some(n) => domain::sqlx::query_scalar("select id from personas where name = $1").bind(n).fetch_optional(&hub.db).await?,
            None => None,
        };
        domain::system::upsert_account(
            hub,
            &contracts::Account {
                id: a.id.clone(),
                company_id: company_id.to_string(),
                platform: a.platform.clone(),
                handle: a.handle.clone(),
                transport: a.transport.clone(),
                persona_id,
                capabilities: a.capabilities.clone(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Register every `registry/comfyui/*.json` or `registry/pipeline/*.json`
/// workflow version it can satisfy.
pub async fn load_registry(hub: &Hub, dir: &Path) -> anyhow::Result<()> {
    for spec in read_registry(dir)? {
        if let Some(engine) = &spec.engine_instance_id {
            let known: Option<String> =
                domain::sqlx::query_scalar("select id from engine_instances where id = $1").bind(engine).fetch_optional(&hub.db).await?;
            if known.is_none() {
                tracing::warn!(workflow = %spec.name, engine = %engine, "engine not registered; skipping workflow");
                continue;
            }
        }
        domain::production::register_workflow(hub, &spec).await.with_context(|| format!("registering {} v{}", spec.name, spec.version))?;
    }
    Ok(())
}

fn read_registry(dir: &Path) -> anyhow::Result<Vec<domain::production::WorkflowSpec>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
    let mut paths: Vec<_> =
        entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    paths.sort();
    // One id, one definition: the pipeline registry is keyed by `pipeline_id`,
    // so a second file would silently win by sort order.
    let mut pipeline_files: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut specs = Vec::with_capacity(paths.len());
    for path in &paths {
        let spec: domain::production::WorkflowSpec =
            serde_json::from_str(&std::fs::read_to_string(path)?).with_context(|| format!("parsing {}", path.display()))?;
        if let Some(id) = &spec.pipeline_id
            && let Some(first) = pipeline_files.insert(id.clone(), path.clone())
        {
            bail!("pipeline id {id:?} is defined by both {} and {}", first.display(), path.display());
        }
        specs.push(spec);
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(name: &'static str) -> Arc<dyn ports::Publisher> {
        Arc::new(Disabled(name))
    }

    #[test]
    fn every_non_fixture_channel_needs_live_paperclip_approvals() {
        let builtin = channel("fixture");
        let aliases = BTreeMap::from([("fixture".to_string(), builtin.clone()), ("mastodon".to_string(), builtin.clone())]);
        assert!(require_live_approvals(AdapterMode::Fixture, AdapterMode::Fixture, &builtin, &aliases).is_ok());

        // A live n8n workflow and a pack channel post for real whatever the
        // built-in publisher mode says.
        let live = BTreeMap::from([
            ("fixture".to_string(), builtin.clone()),
            ("n8n:youtube".to_string(), channel("n8n")),
            ("plinth".to_string(), channel("plinth")),
        ]);
        let err = require_live_approvals(AdapterMode::Fixture, AdapterMode::Fixture, &builtin, &live).unwrap_err().to_string();
        assert!(err.contains("publishing channel"), "the guard explains itself: {err}");
        for transport in ["n8n:youtube", "plinth"] {
            let single = BTreeMap::from([("fixture".to_string(), builtin.clone()), (transport.to_string(), channel(transport))]);
            let err = require_live_approvals(AdapterMode::Fixture, AdapterMode::Fixture, &builtin, &single).unwrap_err().to_string();
            assert!(err.contains(transport), "the failing channel is named: {err}");
        }
        assert!(
            require_live_approvals(AdapterMode::Live, AdapterMode::Fixture, &builtin, &live).is_ok(),
            "live approvals authorize every channel"
        );

        // The disabled publisher simulates like the fixture one, so it needs no
        // live approvals; a pack channel alongside it still does.
        let disabled = channel("disabled");
        let off = BTreeMap::from([("disabled".to_string(), disabled.clone())]);
        assert!(require_live_approvals(AdapterMode::Fixture, AdapterMode::Disabled, &disabled, &off).is_ok());
        let mut mixed = off;
        mixed.insert("plinth".to_string(), channel("plinth"));
        assert!(require_live_approvals(AdapterMode::Fixture, AdapterMode::Disabled, &disabled, &mixed).is_err());
    }

    #[test]
    fn duplicate_builtin_pipeline_ids_fail_startup_naming_both_files() {
        let dir = std::env::temp_dir().join(format!("hub-pipeline-dup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, path) in [("a.json", "f/studio/a"), ("b.json", "f/studio/b")] {
            std::fs::write(
                dir.join(file),
                format!(r#"{{"name":"{file}","version":1,"pipeline_id":"studio.render","windmill_path":"{path}"}}"#),
            )
            .unwrap();
        }
        let err = read_registry(&dir).unwrap_err().to_string();
        assert!(err.contains("a.json") && err.contains("b.json"), "both files are named: {err}");
        assert!(err.contains("studio.render"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
