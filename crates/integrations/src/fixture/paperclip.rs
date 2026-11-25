//! Fixture Paperclip: issues dedupe on the origin key, approvals are decided
//! through the fixture-only board endpoint.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use domain::ports::*;
use serde_json::json;

use super::lock;

#[derive(Default)]
pub struct FixturePaperclip {
    issues: Mutex<HashMap<String, FixtureIssue>>,
    /// Every issue-create request in order, so tests can assert what the hub
    /// asked the studio org to do.
    issue_requests: Mutex<Vec<IssueRequest>>,
    issue_comments: Mutex<Vec<IssueCommentRequest>>,
    issue_updates: Mutex<Vec<IssueUpdateRequest>>,
    routing_error: Mutex<Option<PortError>>,
    approvals: Mutex<HashMap<String, ApprovalView>>,
    /// When set, every call fails as unreachable (fail-closed tests).
    pub unreachable: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone)]
pub struct FixtureIssue {
    pub reference: ForeignRef,
    pub status: String,
    pub priority: Option<String>,
}

impl FixturePaperclip {
    fn check(&self) -> PortResult<()> {
        if self.unreachable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(PortError::Unreachable("fixture paperclip offline".into()));
        }
        Ok(())
    }

    pub fn issue_requests(&self) -> Vec<IssueRequest> {
        lock(&self.issue_requests).clone()
    }

    pub fn issue_comments(&self) -> Vec<IssueCommentRequest> {
        lock(&self.issue_comments).clone()
    }

    pub fn issue_updates(&self) -> Vec<IssueUpdateRequest> {
        lock(&self.issue_updates).clone()
    }

    pub fn issue(&self, id: &str) -> Option<FixtureIssue> {
        lock(&self.issues).values().find(|issue| issue.reference.id == id).cloned()
    }

    pub fn set_routing_error(&self, error: Option<PortError>) {
        *lock(&self.routing_error) = error;
    }

    fn check_issue(&self) -> PortResult<()> {
        self.check()?;
        lock(&self.routing_error).clone().map_or(Ok(()), Err)
    }

    fn missing_issue() -> PortError {
        PortError::Rejected { status: 404, message: "fixture issue not found".into() }
    }

    fn record_comment(&self, req: &IssueCommentRequest) -> ForeignRef {
        let mut comments = lock(&self.issue_comments);
        let existing = req.client_request_id.as_ref().and_then(|id| {
            comments.iter().position(|comment| comment.issue_id == req.issue_id && comment.client_request_id.as_ref() == Some(id))
        });
        let index = existing.unwrap_or_else(|| {
            comments.push(req.clone());
            comments.len() - 1
        });
        ForeignRef { id: format!("fixture-comment-{}", index + 1), url: None }
    }

    /// Board decision in the fixture Paperclip. `user` is the deciding human.
    pub fn decide(&self, approval_id: &str, approve: bool, user: &str) -> PortResult<ApprovalView> {
        let mut a = lock(&self.approvals);
        let v = a.get_mut(approval_id).ok_or(PortError::NotFound)?;
        v.status = if approve { ApprovalStatus::Approved } else { ApprovalStatus::Rejected };
        v.decided_by_user_id = Some(user.to_string());
        v.decided_at = Some(Utc::now());
        v.raw["status"] = json!(v.status);
        Ok(v.clone())
    }

    pub fn approvals(&self) -> Vec<ApprovalView> {
        lock(&self.approvals).values().cloned().collect()
    }
}

#[async_trait]
impl Paperclip for FixturePaperclip {
    async fn create_issue(&self, req: &IssueRequest) -> PortResult<ForeignRef> {
        self.check_issue()?;
        lock(&self.issue_requests).push(req.clone());
        let mut issues = lock(&self.issues);
        let n = issues.len() + 1;
        Ok(issues
            .entry(req.origin_key.clone())
            .or_insert_with(|| FixtureIssue {
                reference: ForeignRef { id: format!("fixture-issue-{n}"), url: Some(format!("fixture://paperclip/issues/{n}")) },
                status: "backlog".into(),
                priority: req.priority.clone(),
            })
            .reference
            .clone())
    }

    async fn comment_issue(&self, req: &IssueCommentRequest) -> PortResult<ForeignRef> {
        self.check_issue()?;
        self.issue(&req.issue_id).ok_or_else(Self::missing_issue)?;
        Ok(self.record_comment(req))
    }

    async fn update_issue(&self, req: &IssueUpdateRequest) -> PortResult<ForeignRef> {
        self.check_issue()?;
        let mut issues = lock(&self.issues);
        let issue = issues.values_mut().find(|issue| issue.reference.id == req.issue_id).ok_or_else(Self::missing_issue)?;
        if let Some(status) = &req.status {
            issue.status = status.clone();
        }
        if let Some(priority) = &req.priority {
            issue.priority = Some(priority.clone());
        }
        if let Some(body) = &req.comment {
            self.record_comment(&IssueCommentRequest {
                issue_id: req.issue_id.clone(),
                body: body.clone(),
                client_request_id: req.comment_client_request_id.clone(),
            });
        }
        lock(&self.issue_updates).push(req.clone());
        Ok(issue.reference.clone())
    }

    async fn verify_routing(&self, _company_id: &str, _project_id: Option<&str>, _assignee_agent_id: Option<&str>) -> PortResult<()> {
        self.check_issue()
    }

    async fn create_approval(&self, req: &ApprovalRequest) -> PortResult<ForeignRef> {
        self.check()?;
        let mut a = lock(&self.approvals);
        if let Some(v) = a.values().find(|v| v.release_revision_id.as_deref() == Some(&req.release_revision_id)) {
            return Ok(ForeignRef { id: v.id.clone(), url: None });
        }
        let id = format!("fixture-approval-{}", a.len() + 1);
        a.insert(
            id.clone(),
            ApprovalView {
                id: id.clone(),
                status: ApprovalStatus::Pending,
                decided_by_user_id: None,
                decided_at: None,
                payload_hash: Some(req.payload_hash.clone()),
                release_revision_id: Some(req.release_revision_id.clone()),
                raw: json!({ "fixture": true, "type": "general", "title": req.title, "payload": { "payloadHash": req.payload_hash } }),
            },
        );
        Ok(ForeignRef { id: id.clone(), url: Some(format!("fixture://paperclip/approvals/{id}")) })
    }

    async fn get_approval(&self, id: &str) -> PortResult<ApprovalView> {
        self.check()?;
        lock(&self.approvals).get(id).cloned().ok_or(PortError::NotFound)
    }
}

#[tokio::test]
async fn fixture_records_issue_mutations_and_simulates_routing_errors() {
    let paperclip = FixturePaperclip::default();
    let request = IssueRequest {
        company_id: "ops".into(),
        title: "Issue".into(),
        description: "fault".into(),
        origin_key: "episode".into(),
        project_id: None,
        assignee_agent_id: None,
        priority: Some("high".into()),
    };
    let issue = paperclip.create_issue(&request).await.unwrap();
    assert_eq!(paperclip.create_issue(&request).await.unwrap(), issue);
    assert_eq!(paperclip.issue(&issue.id).unwrap().priority.as_deref(), Some("high"));
    let comment = IssueCommentRequest { issue_id: issue.id.clone(), body: "New fault".into(), client_request_id: Some("fault".into()) };
    let first = paperclip.comment_issue(&comment).await.unwrap();
    assert_eq!(paperclip.comment_issue(&comment).await.unwrap(), first);
    let update = IssueUpdateRequest {
        issue_id: issue.id.clone(),
        status: Some("done".into()),
        priority: Some("critical".into()),
        comment: Some("Recovered".into()),
        comment_client_request_id: Some("recovery".into()),
        reopen: None,
    };
    assert_eq!(paperclip.update_issue(&update).await.unwrap(), paperclip.update_issue(&update).await.unwrap());
    let state = paperclip.issue(&issue.id).unwrap();
    assert_eq!(state.status, "done");
    assert_eq!(state.priority.as_deref(), Some("critical"));
    assert_eq!(paperclip.issue_comments().iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), ["New fault", "Recovered"]);
    assert_eq!(paperclip.issue_updates().len(), 2, "both requests applied, but only one recovery comment");
    paperclip.set_routing_error(Some(PortError::Rejected { status: 404, message: "bad route".into() }));
    assert!(matches!(paperclip.verify_routing("ops", None, None).await, Err(PortError::Rejected { status: 404, .. })));
    paperclip.set_routing_error(None);
    paperclip.verify_routing("ops", None, None).await.unwrap();
}
