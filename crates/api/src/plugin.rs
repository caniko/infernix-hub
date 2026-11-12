//! Compile-time pack registration for Fleetix Hub.
//!
//! A pack adds a channel, pipeline, outbox topic, `/v1/p/<id>` routes or an
//! integration status entry. Registration is a builder: handlers own cloned
//! `Arc`/`PgPool` dependencies and nothing keeps a borrow of the host. Names are
//! validated at registration and checked again when packs are merged, so a
//! duplicate fails startup instead of winning by registration order.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, ensure};
use axum::Router;
use contracts::IntegrationStatus;
use contracts::client::HubClient;
use domain::outbox::{CORE_TOPICS, OutboxHandler};
use domain::ports::{Pipeline, Publisher};
use domain::sqlx::PgPool;
use rmcp::handler::server::router::tool::ToolRoute;
use serde_json::Value;

use crate::AppState;

/// A compile-time extension of the hub. The upstream binary installs none;
/// `canix-hub` installs its own.
pub trait Pack: Send + Sync {
    /// Pack id, matching `[a-z0-9-]+`; unique across built-ins and packs.
    fn id(&self) -> &'static str;

    fn install(&self, _host: &mut ServerHost<'_>) -> anyhow::Result<()> {
        Ok(())
    }

    fn install_mcp(&self, _host: &mut McpHost<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Everything a pack may need on the server side. Owned and cloneable.
#[derive(Clone)]
pub struct ServerContext {
    pub db: PgPool,
    pub http: reqwest::Client,
    pub public_url: String,
    pub state_dir: PathBuf,
    /// This pack's JSON config from `packs.json` (`{}` when absent).
    pub config: Value,
    /// `$CREDENTIALS_DIRECTORY`.
    pub credentials_dir: PathBuf,
}

/// MCP side: a hub token and this pack's config. No database, no service
/// secrets.
#[derive(Clone)]
pub struct ClientContext {
    pub hub: HubClient,
    pub config: Value,
}

/// Registrations one pack made, collected before the `AppState` exists.
pub(crate) struct PackRegistrations {
    pub pack_id: &'static str,
    pub channels: BTreeMap<String, Arc<dyn Publisher>>,
    pub pipelines: BTreeMap<String, Arc<dyn Pipeline>>,
    pub topics: BTreeMap<String, Arc<dyn OutboxHandler>>,
    pub routes: Option<Router<AppState>>,
    pub integrations: Vec<IntegrationStatus>,
}

/// Builder handed to [`Pack::install`].
pub struct ServerHost<'a> {
    pub ctx: &'a ServerContext,
    pack_id: &'static str,
    channels: BTreeMap<String, Arc<dyn Publisher>>,
    pipelines: BTreeMap<String, Arc<dyn Pipeline>>,
    topics: BTreeMap<String, Arc<dyn OutboxHandler>>,
    routes: Option<Router<AppState>>,
    integrations: Vec<IntegrationStatus>,
}

impl<'a> ServerHost<'a> {
    pub fn new(pack_id: &'static str, ctx: &'a ServerContext) -> Self {
        Self {
            ctx,
            pack_id,
            channels: BTreeMap::new(),
            pipelines: BTreeMap::new(),
            topics: BTreeMap::new(),
            routes: None,
            integrations: Vec::new(),
        }
    }

    /// Register a publishing channel under an `accounts.transport` name.
    pub fn channel(&mut self, transport: &str, publisher: Arc<dyn Publisher>) -> anyhow::Result<()> {
        ensure!(!transport.is_empty(), "pack {}: channel name is empty", self.pack_id);
        ensure!(!self.channels.contains_key(transport), "pack {}: channel {transport} is already registered", self.pack_id);
        self.channels.insert(transport.to_string(), publisher);
        Ok(())
    }

    /// Register a generation pipeline; the id must start with `<pack id>.`.
    pub fn pipeline(&mut self, id: &str, pipeline: Arc<dyn Pipeline>) -> anyhow::Result<()> {
        self.ensure_pack_prefixed(id, "pipeline")?;
        ensure!(!self.pipelines.contains_key(id), "pack {}: pipeline {id} is already registered", self.pack_id);
        self.pipelines.insert(id.to_string(), pipeline);
        Ok(())
    }

    /// Register an outbox topic; pack topics must start with `<pack id>.` and
    /// core topics can never be overridden.
    pub fn topic(&mut self, topic: &str, handler: Arc<dyn OutboxHandler>) -> anyhow::Result<()> {
        self.ensure_pack_prefixed(topic, "topic")?;
        ensure!(!CORE_TOPICS.contains(&topic), "pack {}: topic {topic} is a core topic", self.pack_id);
        ensure!(!self.topics.contains_key(topic), "pack {}: topic {topic} is already registered", self.pack_id);
        self.topics.insert(topic.to_string(), handler);
        Ok(())
    }

    /// Mount a pack router at `/v1/p/<id>`. `AppState` is supplied after
    /// registration, so pack handlers use the same `Caller` extractor as core
    /// routes.
    pub fn routes(&mut self, routes: Router<AppState>) -> anyhow::Result<()> {
        ensure!(self.routes.is_none(), "pack {}: routes are already registered", self.pack_id);
        self.routes = Some(routes);
        Ok(())
    }

    /// Add an integration status entry; the core fixture-vs-live approval guard
    /// stays core-controlled.
    pub fn integration(&mut self, status: IntegrationStatus) {
        self.integrations.push(status);
    }

    fn ensure_pack_prefixed(&self, name: &str, what: &str) -> anyhow::Result<()> {
        let prefix = format!("{}.", self.pack_id);
        ensure!(name.starts_with(&prefix), "pack {}: {what} {name} must start with {prefix}", self.pack_id);
        ensure!(name.len() > prefix.len(), "pack {}: {what} {name} lacks a name after {prefix}", self.pack_id);
        Ok(())
    }

    pub(crate) fn finish(self) -> PackRegistrations {
        PackRegistrations {
            pack_id: self.pack_id,
            channels: self.channels,
            pipelines: self.pipelines,
            topics: self.topics,
            routes: self.routes,
            integrations: self.integrations,
        }
    }
}

/// Builder handed to [`Pack::install_mcp`].
pub struct McpHost<'a> {
    pub ctx: &'a ClientContext,
    server: &'a mut fleetix::mcp_server::Server,
}

impl<'a> McpHost<'a> {
    pub(crate) fn new(ctx: &'a ClientContext, server: &'a mut fleetix::mcp_server::Server) -> Self {
        Self { ctx, server }
    }

    pub fn tool(&mut self, effect: fleetix::mcp_server::Effect, route: ToolRoute<()>) -> anyhow::Result<()> {
        self.server.register(effect, route).map_err(|e| anyhow::anyhow!("mcp tool registration failed: {e:?}"))
    }
}

/// Pack ids are `[a-z0-9-]+`; they also prefix every topic and pipeline id.
pub(crate) fn valid_pack_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Reject pack ids the registration contract cannot address.
pub(crate) fn validate_packs(packs: &[Box<dyn Pack>]) -> anyhow::Result<()> {
    let mut seen = BTreeMap::new();
    for pack in packs {
        let id = pack.id();
        ensure!(valid_pack_id(id), "pack id {id:?} must match [a-z0-9-]+");
        ensure!(!CORE_TOPICS.iter().any(|t| t.split('.').next() == Some(id)), "pack id {id} collides with a core topic namespace");
        if seen.insert(id, ()).is_some() {
            bail!("pack id {id} is registered twice");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use domain::ports::{PortResult, ProviderPost, PublishRequest};
    use serde_json::json;

    struct NoopPublisher;

    #[async_trait::async_trait]
    impl Publisher for NoopPublisher {
        async fn submit(&self, _: &PublishRequest) -> PortResult<ProviderPost> {
            unimplemented!()
        }
        async fn lookup(&self, _: &str, _: &str) -> PortResult<Option<ProviderPost>> {
            unimplemented!()
        }
        async fn reply(&self, _: &domain::ports::ReplyRequest) -> PortResult<ProviderPost> {
            unimplemented!()
        }
        async fn lookup_reply(&self, _: &str, _: &str) -> PortResult<Option<ProviderPost>> {
            unimplemented!()
        }
    }

    fn ctx() -> ServerContext {
        ServerContext {
            db: PgPool::connect_lazy("postgres://nobody@127.0.0.1/none").expect("lazy pool"),
            http: reqwest::Client::new(),
            public_url: "http://127.0.0.1:8470".into(),
            state_dir: "var/state".into(),
            config: json!({}),
            credentials_dir: "var/credentials".into(),
        }
    }

    #[tokio::test]
    async fn pipeline_and_topic_ids_must_be_pack_prefixed_and_unique() {
        let ctx = ctx();
        let mut host = ServerHost::new("studio", &ctx);
        assert!(host.topic("studio.render", Arc::new(NoopTopic)).is_ok());
        assert!(host.topic("studio.render", Arc::new(NoopTopic)).is_err(), "duplicate topic");
        assert!(host.topic("render.done", Arc::new(NoopTopic)).is_err(), "unprefixed topic");
        assert!(host.pipeline("studio.render", Arc::new(NoopPipeline)).is_ok());
        assert!(host.pipeline("studio.render", Arc::new(NoopPipeline)).is_err(), "duplicate pipeline");
        assert!(host.pipeline("render", Arc::new(NoopPipeline)).is_err(), "unprefixed pipeline");
    }

    #[tokio::test]
    async fn core_topics_and_channels_cannot_be_taken() {
        let ctx = ctx();
        let mut host = ServerHost::new("paperclip", &ctx);
        assert!(host.topic("paperclip.create_issue", Arc::new(NoopTopic)).is_err(), "core topic");
        assert!(host.channel("fixture", Arc::new(NoopPublisher)).is_ok());
        assert!(host.channel("fixture", Arc::new(NoopPublisher)).is_err(), "duplicate channel");
        assert!(host.channel("", Arc::new(NoopPublisher)).is_err(), "empty channel");
    }

    #[test]
    fn pack_ids_follow_the_registration_regex() {
        assert!(valid_pack_id("studio"));
        assert!(valid_pack_id("canix-system"));
        assert!(!valid_pack_id("Studio"));
        assert!(!valid_pack_id("studio_pack"));
        assert!(!valid_pack_id(""));

        let duplicate: Vec<Box<dyn Pack>> = vec![Box::new(TestPack), Box::new(TestPack)];
        assert!(validate_packs(&duplicate).is_err(), "duplicate pack ids fail startup");
        let core_namespace: Vec<Box<dyn Pack>> = vec![Box::new(CoreNamedPack)];
        assert!(validate_packs(&core_namespace).is_err(), "a pack may not own a core topic namespace");
    }

    struct CoreNamedPack;
    impl Pack for CoreNamedPack {
        fn id(&self) -> &'static str {
            "paperclip"
        }
    }

    struct NoopTopic;
    #[async_trait::async_trait]
    impl OutboxHandler for NoopTopic {
        async fn deliver(&self, _: &domain::Hub, _: &domain::outbox::OutboxRow) -> domain::outbox::Delivery {
            domain::outbox::Delivery::Done
        }
    }

    struct NoopPipeline;
    #[async_trait::async_trait]
    impl Pipeline for NoopPipeline {
        async fn submit(&self, _: &domain::ports::PipelineRequest) -> PortResult<domain::ports::PipelineRun> {
            unimplemented!()
        }
        async fn status(&self, _: &domain::ports::PipelineRun) -> PortResult<domain::ports::PipelineStatus> {
            unimplemented!()
        }
        async fn cancel(&self, _: &domain::ports::PipelineRun) -> PortResult<domain::ports::CancelOutcome> {
            unimplemented!()
        }
    }

    struct TestPack;

    impl Pack for TestPack {
        fn id(&self) -> &'static str {
            "test-pack"
        }
        fn install(&self, host: &mut ServerHost<'_>) -> anyhow::Result<()> {
            host.routes(Router::new().route("/whoami", axum::routing::get(whoami)).route("/boom", axum::routing::get(boom)))?;
            host.integration(IntegrationStatus {
                name: "test-pack".into(),
                mode: contracts::AdapterMode::Fixture,
                verification: contracts::Verification::FixtureTested,
                detail: "test pack".into(),
            });
            Ok(())
        }
    }

    async fn whoami(crate::Caller(a): crate::Caller) -> Result<Json<serde_json::Value>, crate::ApiErr> {
        a.require(domain::OPERATOR).map_err(|e| crate::ApiErr::domain(e, a.correlation_id.clone()))?;
        Ok(Json(json!({ "name": a.principal.name, "role": a.principal.role.as_str() })))
    }

    async fn boom(crate::Caller(a): crate::Caller) -> Result<Json<serde_json::Value>, crate::ApiErr> {
        a.require(domain::OPERATOR).map_err(|e| crate::ApiErr::domain(e, a.correlation_id.clone()))?;
        Err(crate::ApiErr::domain(domain::DomainError::Invalid { code: "boom", message: "expected".into() }, a.correlation_id))
    }

    async fn fresh_db() -> Option<String> {
        let Ok(base) = std::env::var("HUB_TEST_DATABASE_URL") else {
            eprintln!("HUB_TEST_DATABASE_URL unset; skipping DB-backed test");
            return None;
        };
        let admin = PgPool::connect(&base).await.expect("connect admin db");
        let name = format!("t_{}", uuid::Uuid::new_v4().simple());
        domain::sqlx::query(&format!("create database {name}")).execute(&admin).await.unwrap();
        let (head, query) = base.split_once('?').map(|(h, q)| (h.to_string(), format!("?{q}"))).unwrap_or((base.clone(), String::new()));
        let head = head.rsplit_once('/').map(|(h, _)| h.to_string()).unwrap();
        Some(format!("{head}/{name}{query}"))
    }

    #[tokio::test]
    async fn pack_route_mounts_under_v1_p_with_the_core_caller_check() {
        let Some(db_url) = fresh_db().await else { return };
        let assets = std::env::temp_dir().join(format!("hub-pack-test-{}", uuid::Uuid::new_v4()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let mut s = crate::bootstrap::Settings::fixture(&db_url, &addr);
        s.asset_dir = assets.clone();
        s.registry_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry").into();
        s.seed_file = Some(concat!(env!("CARGO_MANIFEST_DIR"), "/../../registry/hub.fixture.json").into());
        let (state, _fx) = crate::bootstrap::build(&s).await.unwrap();
        for (name, role, token) in [("operator", contracts::Role::Operator, "op"), ("agent", contracts::Role::Agent, "ag")] {
            domain::system::issue_token(&state.hub.db, name, role, Some(token.into())).await.unwrap();
        }
        let ctx = ServerContext {
            db: state.hub.db.clone(),
            http: reqwest::Client::new(),
            public_url: s.public_url.clone(),
            state_dir: assets.clone(),
            config: json!({ "enabled": true }),
            credentials_dir: assets.join("credentials"),
        };
        let mut host = ServerHost::new("test-pack", &ctx);
        TestPack.install(&mut host).unwrap();
        let regs = host.finish();
        assert_eq!(regs.integrations.len(), 1, "pack status entry is recorded");
        let routes = regs.routes.expect("routes registered");
        let server = tokio::spawn(async move {
            axum::serve(listener, crate::router_with(state, vec![("test-pack".into(), routes)])).await.unwrap();
        });
        let base = format!("http://{addr}");
        let op = contracts::client::HubClient::new(&base, "op");
        let agent = contracts::client::HubClient::new(&base, "ag");
        let body: serde_json::Value = op.get("/p/test-pack/whoami").await.unwrap();
        assert_eq!(body["name"], "operator");
        let denied = agent.get::<serde_json::Value>("/p/test-pack/whoami").await.unwrap_err();
        assert!(matches!(denied, contracts::client::ClientError::Api { status: 403, .. }), "role check: {denied}");
        let missing = reqwest::get(format!("{base}/v1/p/test-pack/whoami")).await.unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::FORBIDDEN);
        let coded = op.get::<serde_json::Value>("/p/test-pack/boom").await.unwrap_err();
        assert!(matches!(coded, contracts::client::ClientError::Api { status: 422, .. }), "pack errors keep the core shape: {coded}");
        assert!(op.get::<contracts::SystemStatus>("/status").await.is_ok(), "core routes still work");
        server.abort();
    }
}
