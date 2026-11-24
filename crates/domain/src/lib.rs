//! Hub domain: the one command layer behind the HTTP API, the worker, the CLI
//! and agent tools. Every mutation goes through a function here that checks
//! the caller's role, changes state and records outbound commands in one
//! transaction.

use std::collections::BTreeMap;
use std::sync::Arc;

use contracts::{Principal, Role};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::PgPool;

pub mod canon;
pub mod content;
pub mod engagement;
pub mod feeds;
pub mod hash;
pub mod look;
pub mod news;
pub mod notifications;
pub mod opportunities;
pub mod outbox;
pub mod ports;
pub mod production;
pub mod projects;
pub mod providers;
pub mod release;
pub mod renewals;
pub mod system;

pub use sqlx;

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("{code}: {message}")]
    Conflict { code: &'static str, message: String },
    #[error("{code}: {message}")]
    Invalid { code: &'static str, message: String },
    /// A required external read failed; we fail closed.
    #[error("{code}: {message}")]
    Unavailable { code: &'static str, message: String },
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("internal: {0}")]
    Internal(String),
}

impl DomainError {
    pub fn code(&self) -> &'static str {
        match self {
            DomainError::Forbidden(_) => "forbidden",
            DomainError::NotFound(_) => "not_found",
            DomainError::Conflict { code, .. } | DomainError::Invalid { code, .. } | DomainError::Unavailable { code, .. } => code,
            DomainError::Db(_) => "database",
            DomainError::Internal(_) => "internal",
        }
    }
}

pub fn conflict(code: &'static str, message: impl Into<String>) -> DomainError {
    DomainError::Conflict { code, message: message.into() }
}

pub fn invalid(code: &'static str, message: impl Into<String>) -> DomainError {
    DomainError::Invalid { code, message: message.into() }
}

pub type DResult<T> = Result<T, DomainError>;

/// Static configuration the domain needs at runtime.
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// URL external procedures use to reach this hub (receipts, dispatch).
    pub public_url: String,
    pub company_id: String,
    /// Adapter name → (mode, verification, detail) for the status report.
    pub integrations: Vec<contracts::IntegrationStatus>,
    /// How early a timer may fire relative to `due_at` (clock skew).
    pub dispatch_early_secs: i64,
    /// How long after `due_at` a dispatch is still within its window.
    pub dispatch_window_secs: i64,
    /// Where fleet degradations are filed as Paperclip issues; unset files none.
    pub fleet_issues: Option<FleetIssues>,
    /// How long a daily summary stays fresh; unset disables the stale alert.
    pub summary_stale_hours: Option<u32>,
}

/// Paperclip routing for fleet notifications (the operations company, its
/// project and the agent that starts the fix).
#[derive(Debug, Clone, PartialEq)]
pub struct FleetIssues {
    pub company_id: String,
    pub project_id: Option<String>,
    pub assignee_agent_id: Option<String>,
}

#[derive(Clone)]
pub struct Hub {
    pub db: PgPool,
    pub config: Arc<HubConfig>,
    pub paperclip: Arc<dyn ports::Paperclip>,
    pub windmill: Arc<dyn ports::Windmill>,
    pub comfy: Arc<dyn ports::Comfy>,
    pub publisher: Arc<dyn ports::Publisher>,
    pub assets: Arc<dyn ports::AssetStore>,
    pub assistant: Arc<dyn ports::Assistant>,
    pub gpu_peer: Arc<dyn ports::GpuPeer>,
    /// Publishing channels keyed by `accounts.transport`; dispatch uses only
    /// the transport frozen into the release action.
    pub channels: Arc<BTreeMap<String, Arc<dyn ports::Publisher>>>,
    /// Generation pipelines keyed by `workflows.pipeline_id`.
    pub pipelines: Arc<BTreeMap<String, Arc<dyn ports::Pipeline>>>,
    /// Pack-provided outbox topics. Core topics stay in the worker's match.
    pub topics: Arc<BTreeMap<String, Arc<dyn outbox::OutboxHandler>>>,
    /// Registered AI model providers (built-ins, fixtures, and pack providers).
    pub providers: Arc<BTreeMap<String, Arc<dyn ports::ModelProvider>>>,
    /// Wakes long-polling notification readers after a committed change.
    pub notified: Arc<tokio::sync::Notify>,
    /// Expected-offline hosts from the latest fleet snapshot read by the API's notifier.
    pub expected_offline_hosts: Arc<std::sync::RwLock<Vec<String>>>,
    /// Status feed states the producer loop last saw, for `/v1/status`.
    pub feeds: Arc<std::sync::RwLock<Vec<contracts::FeedState>>>,
}

/// The authenticated caller plus the request's correlation id.
#[derive(Debug, Clone)]
pub struct Actor {
    pub principal: Principal,
    pub correlation_id: Option<String>,
}

impl Actor {
    pub fn require(&self, roles: &[Role]) -> DResult<()> {
        if roles.contains(&self.principal.role) {
            Ok(())
        } else {
            Err(DomainError::Forbidden(format!("role {} may not perform this action", self.principal.role)))
        }
    }

    pub fn name(&self) -> String {
        format!("{}:{}", self.principal.role, self.principal.name)
    }

    /// The internal worker identity.
    pub fn system() -> Self {
        Actor { principal: Principal { id: uuid::Uuid::nil(), name: "worker".into(), role: Role::Procedure }, correlation_id: None }
    }
}

pub const ANY_READER: &[Role] = &[Role::Operator, Role::Agent];
pub const OPERATOR: &[Role] = &[Role::Operator];
pub const OPERATOR_OR_AGENT: &[Role] = &[Role::Operator, Role::Agent];
/// The procedure role is a machine caller; observations are its only renewal
/// write, and never a rotation.
pub const OPERATOR_OR_PROCEDURE: &[Role] = &[Role::Operator, Role::Procedure];
/// Collectors authenticate per `(host, unix_user)` for telemetry and operation receipts.
pub const OPERATOR_OR_COLLECTOR: &[Role] = &[Role::Operator, Role::Collector];

/// Decode rows produced by `select to_jsonb(t) from (...) t` into contract
/// types. Column names are the contract field names.
pub(crate) fn decode<T: DeserializeOwned>(rows: Vec<Value>) -> DResult<Vec<T>> {
    rows.into_iter().map(|v| serde_json::from_value(v).map_err(|e| DomainError::Internal(format!("decode: {e}")))).collect()
}

pub(crate) fn decode_one<T: DeserializeOwned>(row: Value) -> DResult<T> {
    serde_json::from_value(row).map_err(|e| DomainError::Internal(format!("decode: {e}")))
}

/// Append to the audit log inside the caller's transaction.
pub(crate) async fn audit(
    tx: &mut sqlx::PgConnection,
    actor: &Actor,
    action: &str,
    subject_type: &str,
    subject_id: impl ToString,
    detail: Value,
) -> DResult<()> {
    sqlx::query(
        "insert into audit_log (actor, action, subject_type, subject_id, correlation_id, detail)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(actor.name())
    .bind(action)
    .bind(subject_type)
    .bind(subject_id.to_string())
    .bind(&actor.correlation_id)
    .bind(detail)
    .execute(tx)
    .await?;
    Ok(())
}

pub async fn history(db: &PgPool, subject_type: &str, subject_id: &str) -> DResult<Vec<contracts::HistoryEntry>> {
    // Fail closed: only an existing ownerless renewal has readable history, so a
    // member's renewal stays hidden even after its row is gone.
    if subject_type == "renewal" {
        let public: bool = sqlx::query_scalar("select exists(select 1 from renewals where id::text = $1 and owner is null)")
            .bind(subject_id)
            .fetch_one(db)
            .await?;
        if !public {
            return Err(DomainError::NotFound("renewal"));
        }
    }
    // A member's own acknowledgements and snoozes name rows the operator may not
    // see (its renewal notifications), so notification history omits them.
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(a) from (select id, at, actor, action, subject_type, subject_id, correlation_id, detail
         from audit_log where subject_type = $1 and subject_id = $2
           and not ($1 = 'notification' and actor like 'member:%')
         order by id desc limit 200) a",
    )
    .bind(subject_type)
    .bind(subject_id)
    .fetch_all(db)
    .await?;
    decode(rows)
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Embedded migrations not yet applied successfully (all of them on an empty database).
pub async fn pending_migrations(db: &PgPool) -> DResult<Vec<i64>> {
    let applied = applied_migrations(db).await?;
    Ok(MIGRATOR.iter().map(|m| m.version).filter(|v| !applied.contains(v)).collect())
}

pub async fn applied_migrations(db: &PgPool) -> DResult<Vec<i64>> {
    let exists: bool = sqlx::query_scalar("select to_regclass('_sqlx_migrations') is not null").fetch_one(db).await?;
    let applied: Vec<i64> = if exists {
        sqlx::query_scalar("select version from _sqlx_migrations where success order by version").fetch_all(db).await?
    } else {
        vec![]
    };
    Ok(applied)
}
