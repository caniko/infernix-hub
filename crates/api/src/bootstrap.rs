//! Configuration from the environment, adapter selection and registry seeding.
//!
//! Each integration is independently `fixture` or `live`. Secrets are read
//! from `*_TOKEN_FILE` paths (systemd credentials / agenix), never from argv,
//! and never leave the server.

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
    pub company_id: Option<String>,
    pub paperclip: AdapterMode,
    pub windmill: AdapterMode,
    pub comfy: AdapterMode,
    pub publisher: AdapterMode,
    pub assistant: AdapterMode,
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
            company_id: env("HUB_COMPANY_ID"),
            paperclip: mode("HUB_PAPERCLIP")?,
            windmill: mode("HUB_WINDMILL")?,
            comfy: mode("HUB_COMFY")?,
            publisher: mode("HUB_PUBLISHER")?,
            assistant: mode("HUB_ASSISTANT")?,
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
            company_id: None,
            paperclip: AdapterMode::Fixture,
            windmill: AdapterMode::Fixture,
            comfy: AdapterMode::Fixture,
            publisher: AdapterMode::Fixture,
            assistant: AdapterMode::Fixture,
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

pub async fn build(s: &Settings) -> anyhow::Result<(AppState, Fixtures)> {
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
            bail!("database has pending migrations {pending:?}; run `infernix-hubd migrate` first");
        }
    } else {
        domain::MIGRATOR.run(&db).await.context("running migrations")?;
    }
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let mut fx = Fixtures::default();

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
        AdapterMode::Live => Arc::new(integrations::live::WindmillClient::new(
            http.clone(),
            integrations::live::WindmillConfig::new(
                env("WINDMILL_URL").unwrap_or_else(|| "http://127.0.0.1:8040".into()),
                env("WINDMILL_WORKSPACE").unwrap_or_else(|| "studio".into()),
                secret("WINDMILL_TOKEN")?,
            ),
        )),
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
    };
    if let Some(seed) = &seed {
        apply_seed(&hub, seed, &hub.config.company_id).await?;
    }
    load_registry(&hub, &s.registry_dir.join("comfyui")).await?;
    Ok((AppState { hub, fixture_paperclip: fx.paperclip.clone() }, fx))
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

/// Register every `registry/comfyui/*.json` workflow version.
pub async fn load_registry(hub: &Hub, dir: &Path) -> anyhow::Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
    let mut paths: Vec<_> =
        entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    paths.sort();
    for p in paths {
        let spec: domain::production::WorkflowSpec =
            serde_json::from_str(&std::fs::read_to_string(&p)?).with_context(|| format!("parsing {}", p.display()))?;
        let engine: Option<String> = domain::sqlx::query_scalar("select id from engine_instances where id = $1")
            .bind(&spec.engine_instance_id)
            .fetch_optional(&hub.db)
            .await?;
        if engine.is_none() {
            tracing::warn!(workflow = %spec.name, engine = %spec.engine_instance_id, "engine not registered; skipping workflow");
            continue;
        }
        domain::production::register_workflow(hub, &spec).await.with_context(|| format!("registering {}", p.display()))?;
    }
    Ok(())
}
