//! Paperclip board adapter (0.3.1, API schema 1.0.0).
//!
//! Contract source: `docs/contracts/paperclip-openapi-subset.json`, extracted
//! from the live `GET /api/openapi.json` (823 paths) on 2026-11-09 for source
//! commit `6f1bf1754c4862cbae59a58306652ded8a44cd17`. The OpenAPI leaves the
//! issue-create body untyped; field names below come from that commit's
//! `createIssueSchema`/`createApprovalSchema` (see docs/capabilities.md).
//!
//! Idempotency: issue create accepts `idempotencyKey` (max 255 chars) and the
//! server replays the original issue for a repeated key, so `origin_key` is
//! sent there. Approval create has no idempotency field in 0.3.1; the hub must
//! reconcile by reading the approval it recorded.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use domain::ports::{ApprovalRequest, ApprovalStatus, ApprovalView, ForeignRef, IssueRequest, Paperclip, PortError, PortResult};
use serde_json::{Value, json};

use super::{read_json, transport_error, trim_base};

/// Paperclip connection settings. `Debug` is manual so the board key cannot
/// leak through `{:?}`, tracing fields or error contexts.
#[derive(Clone)]
pub struct PaperclipConfig {
    /// Origin of the Paperclip deployment, e.g. `http://127.0.0.1:3117`.
    pub base_url: String,
    /// Board API key (`Authorization: Bearer …`).
    pub api_key: String,
}

impl PaperclipConfig {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self { base_url: trim_base(&base_url.into()), api_key: api_key.into() }
    }
}

impl std::fmt::Debug for PaperclipConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaperclipConfig").field("base_url", &self.base_url).field("api_key", &"<redacted>").finish()
    }
}

pub struct PaperclipClient {
    http: reqwest::Client,
    config: PaperclipConfig,
}

impl PaperclipClient {
    pub fn new(http: reqwest::Client, config: PaperclipConfig) -> Self {
        Self { http, config }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url, path)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.config.api_key)
    }
}

/// Map Paperclip's approval lifecycle
/// (`pending | revision_requested | approved | rejected | cancelled`) onto the
/// hub's vocabulary. Unknown values fail closed instead of guessing.
fn map_status(status: &str) -> PortResult<ApprovalStatus> {
    match status {
        "pending" => Ok(ApprovalStatus::Pending),
        "approved" => Ok(ApprovalStatus::Approved),
        "rejected" => Ok(ApprovalStatus::Rejected),
        "revision_requested" => Ok(ApprovalStatus::RevisionRequested),
        "cancelled" | "canceled" => Ok(ApprovalStatus::Cancelled),
        other => Err(PortError::Unsupported(format!("paperclip approval status `{other}` is not mapped"))),
    }
}

fn payload_str(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let text = value?.as_str()?;
    DateTime::parse_from_rfc3339(text).ok().map(|dt| dt.with_timezone(&Utc))
}

/// Create-issue body. Project and assignee are sent only when the hub knows
/// them, so an unconfigured deployment keeps the previous request shape.
fn issue_body(req: &IssueRequest) -> Value {
    let mut body = json!({
        "title": req.title,
        "description": req.description,
        // Server-supported idempotency: a retried create with the same key
        // replays the original issue instead of making a duplicate.
        "idempotencyKey": req.origin_key,
    });
    if let Some(project) = &req.project_id {
        body["projectId"] = json!(project);
    }
    if let Some(agent) = &req.assignee_agent_id {
        body["assigneeAgentId"] = json!(agent);
    }
    body
}

#[async_trait]
impl Paperclip for PaperclipClient {
    async fn create_issue(&self, req: &IssueRequest) -> PortResult<ForeignRef> {
        let url = self.url(&format!("/api/companies/{}/issues", req.company_id));
        let body = issue_body(req);
        let resp = self.auth(self.http.post(url)).json(&body).send().await.map_err(|e| transport_error(e, true))?;
        let value = read_json(resp, false, true).await?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| PortError::Ambiguous("paperclip create issue response has no id".into()))?
            .to_string();
        let url = value.get("url").and_then(Value::as_str).map(str::to_string);
        Ok(ForeignRef { id, url })
    }

    async fn create_approval(&self, req: &ApprovalRequest) -> PortResult<ForeignRef> {
        let url = self.url(&format!("/api/companies/{}/approvals", req.company_id));
        let mut payload = json!({
            "title": req.title,
            "summary": req.summary,
            // The exact revision and hash the human is approving. The hub
            // re-reads these from `get_approval` before acting.
            "release_revision_id": req.release_revision_id,
            "payload_hash": req.payload_hash,
            "actions": req.actions,
        });
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("requested_by".into(), json!("fleetix-hub"));
        }
        let mut body = json!({
            // The general board approval type supported by Paperclip 0.3.1.
            "type": "request_board_approval",
            "payload": payload,
        });
        if let Some(issue_id) = &req.issue_id {
            body["issueIds"] = json!([issue_id]);
        }
        let resp = self.auth(self.http.post(url)).json(&body).send().await.map_err(|e| transport_error(e, true))?;
        let value = read_json(resp, false, true).await?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| PortError::Ambiguous("paperclip create approval response has no id".into()))?
            .to_string();
        let url = value.get("url").and_then(Value::as_str).map(str::to_string);
        Ok(ForeignRef { id, url })
    }

    async fn get_approval(&self, id: &str) -> PortResult<ApprovalView> {
        let url = self.url(&format!("/api/approvals/{id}"));
        let resp = self.auth(self.http.get(url)).send().await.map_err(|e| transport_error(e, false))?;
        let value = read_json(resp, true, false).await?;
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| PortError::Rejected { status: 200, message: "paperclip approval response has no status".into() })?;
        let payload = value.get("payload").cloned().unwrap_or(Value::Null);
        Ok(ApprovalView {
            id: value.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| id.to_string()),
            status: map_status(status)?,
            // Paperclip records the deciding human board user here; the API
            // key's identity is never used as the decider.
            decided_by_user_id: payload_str(&value, "decidedByUserId"),
            decided_at: parse_timestamp(value.get("decidedAt")),
            payload_hash: payload_str(&payload, "payload_hash"),
            release_revision_id: payload_str(&payload, "release_revision_id"),
            raw: value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PaperclipConfig {
        PaperclipConfig::new("http://127.0.0.1:3117/", "board-secret-key")
    }

    #[test]
    fn debug_redacts_the_board_key() {
        let dbg = format!("{:?}", config());
        assert!(!dbg.contains("board-secret-key"), "secret leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains("http://127.0.0.1:3117"));
    }

    #[test]
    fn base_url_trailing_slashes_are_trimmed() {
        assert_eq!(config().base_url, "http://127.0.0.1:3117");
    }

    #[test]
    fn issue_body_carries_project_and_assignee_when_set() {
        let mut req = IssueRequest {
            company_id: "studio".into(),
            title: "Kiln story".into(),
            description: "brief".into(),
            origin_key: "fleetix-hub:content:1".into(),
            project_id: None,
            assignee_agent_id: None,
        };
        let body = issue_body(&req);
        assert_eq!(body["idempotencyKey"], "fleetix-hub:content:1");
        assert!(body.get("projectId").is_none() && body.get("assigneeAgentId").is_none(), "unset ids are not sent");
        req.project_id = Some("project_studio_content".into());
        req.assignee_agent_id = Some("agent_studio_showrunner".into());
        let body = issue_body(&req);
        assert_eq!(body["projectId"], "project_studio_content");
        assert_eq!(body["assigneeAgentId"], "agent_studio_showrunner");
    }

    #[test]
    fn approval_statuses_map_exactly() {
        assert_eq!(map_status("pending").unwrap(), ApprovalStatus::Pending);
        assert_eq!(map_status("approved").unwrap(), ApprovalStatus::Approved);
        assert_eq!(map_status("rejected").unwrap(), ApprovalStatus::Rejected);
        assert_eq!(map_status("revision_requested").unwrap(), ApprovalStatus::RevisionRequested);
        assert_eq!(map_status("cancelled").unwrap(), ApprovalStatus::Cancelled);
        assert!(matches!(map_status("weird"), Err(PortError::Unsupported(_))));
    }

    #[test]
    fn approval_view_is_decoded_from_the_contract_shape() {
        let raw: Value = serde_json::from_str(include_str!("testdata/paperclip-approval.json")).unwrap();
        let payload = raw.get("payload").cloned().unwrap();
        let view = ApprovalView {
            id: raw["id"].as_str().unwrap().to_string(),
            status: map_status(raw["status"].as_str().unwrap()).unwrap(),
            decided_by_user_id: payload_str(&raw, "decidedByUserId"),
            decided_at: parse_timestamp(raw.get("decidedAt")),
            payload_hash: payload_str(&payload, "payload_hash"),
            release_revision_id: payload_str(&payload, "release_revision_id"),
            raw,
        };
        assert_eq!(view.status, ApprovalStatus::Approved);
        assert_eq!(view.decided_by_user_id.as_deref(), Some("user_board_human"));
        assert_eq!(view.payload_hash.as_deref(), Some("sha256:abc"));
        assert_eq!(view.release_revision_id.as_deref(), Some("relrev_01"));
        assert!(view.decided_at.is_some());
    }
}
