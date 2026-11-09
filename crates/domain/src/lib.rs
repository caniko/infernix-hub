//! Hub domain: the one command layer behind the HTTP API, the worker, the CLI
//! and agent tools. Every mutation goes through a function here that checks
//! the caller's role, changes state and records outbound commands in one
//! transaction.

use std::sync::Arc;

use contracts::{Principal, Role};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::PgPool;

pub mod canon;
pub mod content;
pub mod engagement;
pub mod hash;
pub mod news;
pub mod outbox;
pub mod ports;
pub mod production;
pub mod release;
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
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(a) from (select id, at, actor, action, subject_type, subject_id, correlation_id, detail
         from audit_log where subject_type = $1 and subject_id = $2 order by id desc limit 200) a",
    )
    .bind(subject_type)
    .bind(subject_id)
    .fetch_all(db)
    .await?;
    decode(rows)
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
