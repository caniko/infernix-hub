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
use domain::ports::{
    ApprovalRequest, ApprovalStatus, ApprovalView, ForeignRef, IssueCommentRequest, IssueRequest, IssueUpdateRequest, Paperclip, PortError,
    PortResult,
};
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

    async fn issue_write(&self, request: reqwest::RequestBuilder, body: &Value, operation: &str) -> PortResult<(ForeignRef, Value)> {
        let resp = self.auth(request).json(body).send().await.map_err(|e| transport_error(e, true))?;
        let value = read_json(resp, false, true).await?;
        let id = payload_str(&value, "id")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| PortError::Ambiguous(format!("paperclip {operation} response has no id")))?;
        Ok((ForeignRef { id, url: payload_str(&value, "url") }, value))
    }

    async fn routing_resource(&self, collection: &str, id: &str) -> PortResult<Value> {
        let url = self.url(&format!("/api/{collection}/{id}"));
        let resp = self.auth(self.http.get(url)).send().await.map_err(|e| transport_error(e, false))?;
        read_json(resp, false, false).await.map_err(|err| match err {
            PortError::Rejected { status, message } => {
                PortError::Rejected { status, message: format!("Paperclip {collection}/{id}: {message}") }
            }
            other => other,
        })
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_id(id: &str, field: &str) -> PortResult<()> {
    if !safe_id(id) {
        return Err(PortError::Rejected { status: 400, message: format!("Invalid Paperclip {field}") });
    }
    Ok(())
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
    if let Some(priority) = &req.priority {
        body["priority"] = json!(priority);
    }
    body
}

#[async_trait]
impl Paperclip for PaperclipClient {
    async fn create_issue(&self, req: &IssueRequest) -> PortResult<ForeignRef> {
        validate_id(&req.company_id, "company ID")?;
        let url = self.url(&format!("/api/companies/{}/issues", req.company_id));
        let (mut issue, value) = self.issue_write(self.http.post(url), &issue_body(req), "create issue").await?;
        if issue.url.is_none() {
            issue.url = value["identifier"].as_str().filter(|id| safe_id(id)).map(|id| self.url(&format!("/issues/{id}")));
        }
        Ok(issue)
    }

    async fn comment_issue(&self, req: &IssueCommentRequest) -> PortResult<ForeignRef> {
        validate_id(&req.issue_id, "issue ID")?;
        let url = self.url(&format!("/api/issues/{}/comments", req.issue_id));
        let mut body = json!(req);
        body.as_object_mut().expect("issue comment serializes to an object").remove("issue_id");
        self.issue_write(self.http.post(url), &body, "comment").await.map(|(r, _)| r)
    }

    async fn update_issue(&self, req: &IssueUpdateRequest) -> PortResult<ForeignRef> {
        validate_id(&req.issue_id, "issue ID")?;
        let url = self.url(&format!("/api/issues/{}", req.issue_id));
        let mut body = json!(req);
        body.as_object_mut().expect("issue update serializes to an object").remove("issue_id");
        self.issue_write(self.http.patch(url), &body, "update issue").await.map(|(r, _)| r)
    }

    async fn verify_routing(&self, company_id: &str, project_id: Option<&str>, assignee_agent_id: Option<&str>) -> PortResult<()> {
        validate_id(company_id, "company ID")?;
        for (collection, id) in [("projects", project_id), ("agents", assignee_agent_id)] {
            if let Some(id) = id {
                validate_id(id, collection)?;
            }
        }
        self.routing_resource("companies", company_id).await?;
        for (collection, id) in [("projects", project_id), ("agents", assignee_agent_id)] {
            if let Some(id) = id
                && self.routing_resource(collection, id).await?["companyId"].as_str() != Some(company_id)
            {
                return Err(PortError::Rejected {
                    status: 400,
                    message: format!("Paperclip {collection}/{id} does not belong to company {company_id}"),
                });
            }
        }
        Ok(())
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
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

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
            priority: None,
        };
        let body = issue_body(&req);
        assert_eq!(body["idempotencyKey"], "fleetix-hub:content:1");
        assert!(body.get("projectId").is_none() && body.get("assigneeAgentId").is_none(), "unset ids are not sent");
        assert!(body.get("priority").is_none());
        req.project_id = Some("project_studio_content".into());
        req.assignee_agent_id = Some("agent_studio_showrunner".into());
        req.priority = Some("critical".into());
        let body = issue_body(&req);
        assert_eq!(body["projectId"], "project_studio_content");
        assert_eq!(body["assigneeAgentId"], "agent_studio_showrunner");
        assert_eq!(body["priority"], "critical");
    }

    async fn server(calls: Vec<(&'static str, Value, u16, Value)>) -> (PaperclipClient, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cfg = PaperclipConfig::new(format!("http://{}", listener.local_addr().unwrap()), "fixture-key");
        let client = PaperclipClient::new(reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build().unwrap(), cfg);
        let server = tokio::spawn(async move {
            for (target, expected, status, response) in calls {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(stream);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert_eq!(line, format!("{target} HTTP/1.1\r\n"));
                let (mut len, mut auth) = (0, false);
                loop {
                    line.clear();
                    assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            len = value.trim().parse().unwrap();
                        } else if name.eq_ignore_ascii_case("authorization") {
                            auth = value.trim() == "Bearer fixture-key";
                        }
                    }
                }
                assert!(auth);
                let mut body = vec![0; len];
                socket.read_exact(&mut body).await.unwrap();
                if expected.is_null() {
                    assert!(body.is_empty());
                } else {
                    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected);
                }
                let response = response.to_string();
                let wire = format!(
                    "HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                    response.len()
                );
                socket.get_mut().write_all(wire.as_bytes()).await.unwrap();
            }
        });
        (client, server)
    }

    #[tokio::test]
    async fn issue_calls_send_priority_comments_and_recovery_patch() {
        let comment_id = "11111111-1111-4111-8111-111111111111";
        let update_id = "22222222-2222-4222-8222-222222222222";
        let (client, server) = server(vec![
            (
                "POST /api/companies/ops/issues",
                json!({"title":"Broken","description":"disk","idempotencyKey":"ep","priority":"high"}),
                201,
                json!({"id":"issue", "identifier":"OPS-42"}),
            ),
            (
                "POST /api/companies/ops/issues",
                json!({"title":"Broken","description":"disk","idempotencyKey":"ep","priority":"high"}),
                201,
                json!({"id":"issue"}),
            ),
            ("POST /api/issues/issue/comments", json!({"body":"Added: GPU","clientRequestId":comment_id}), 201, json!({"id":"comment"})),
            (
                "PATCH /api/issues/issue",
                json!({"status":"done","priority":"medium","comment":"Recovered","commentClientRequestId":update_id,"reopen":false}),
                200,
                json!({"id":"issue"}),
            ),
            ("PATCH /api/issues/issue", json!({"priority":"low"}), 200, json!({"id":"issue"})),
        ])
        .await;
        let req = IssueRequest {
            company_id: "ops".into(),
            title: "Broken".into(),
            description: "disk".into(),
            origin_key: "ep".into(),
            project_id: None,
            assignee_agent_id: None,
            priority: Some("high".into()),
        };
        let issue = client.create_issue(&req).await.unwrap();
        assert_eq!(issue.url, Some(client.url("/issues/OPS-42")));
        assert_eq!(client.create_issue(&req).await.unwrap().url, None);
        let comment = client
            .comment_issue(&IssueCommentRequest {
                issue_id: issue.id.clone(),
                body: "Added: GPU".into(),
                client_request_id: Some(comment_id.into()),
            })
            .await
            .unwrap();
        assert_eq!(comment.id, "comment");
        let updated = client
            .update_issue(&IssueUpdateRequest {
                issue_id: issue.id,
                status: Some("done".into()),
                priority: Some("medium".into()),
                comment: Some("Recovered".into()),
                comment_client_request_id: Some(update_id.into()),
                reopen: Some(false),
            })
            .await
            .unwrap();
        assert_eq!(updated.id, "issue");
        let priority = serde_json::from_value(json!({"issue_id": updated.id, "priority": "low"})).unwrap();
        client.update_issue(&priority).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn routing_checks_ownership_and_rejects_invalid_or_missing_ids() {
        let (client, server) = server(vec![
            ("GET /api/companies/ops", Value::Null, 200, json!({"id":"ops"})),
            ("GET /api/projects/project", Value::Null, 200, json!({"companyId":"ops"})),
            ("GET /api/agents/agent", Value::Null, 200, json!({"companyId":"ops"})),
            ("GET /api/companies/missing", Value::Null, 404, json!({"error":"not found"})),
            ("GET /api/companies/ops", Value::Null, 200, json!({"id":"ops"})),
            ("GET /api/projects/other", Value::Null, 200, json!({"companyId":"studio"})),
            ("GET /api/companies/ops", Value::Null, 200, json!({"id":"ops"})),
            ("GET /api/agents/other", Value::Null, 200, json!({"companyId":"studio"})),
        ])
        .await;
        assert!(matches!(client.verify_routing("ops", Some("../project"), None).await, Err(PortError::Rejected { status: 400, .. })));
        client.verify_routing("ops", Some("project"), Some("agent")).await.unwrap();
        assert!(matches!(client.verify_routing("missing", None, None).await, Err(PortError::Rejected { status: 404, .. })));
        for (project, agent) in [(Some("other"), None), (None, Some("other"))] {
            assert!(matches!(
                client.verify_routing("ops", project, agent).await,
                Err(PortError::Rejected {status: 400, message}) if message.contains("does not belong to company ops")
            ));
        }
        server.await.unwrap();
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
