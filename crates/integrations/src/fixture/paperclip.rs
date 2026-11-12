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
    issues: Mutex<HashMap<String, ForeignRef>>,
    /// Every issue-create request in order, so tests can assert what the hub
    /// asked the studio org to do.
    issue_requests: Mutex<Vec<IssueRequest>>,
    approvals: Mutex<HashMap<String, ApprovalView>>,
    /// When set, every call fails as unreachable (fail-closed tests).
    pub unreachable: std::sync::atomic::AtomicBool,
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
        self.check()?;
        lock(&self.issue_requests).push(req.clone());
        let mut issues = lock(&self.issues);
        let n = issues.len() + 1;
        Ok(issues
            .entry(req.origin_key.clone())
            .or_insert_with(|| ForeignRef { id: format!("fixture-issue-{n}"), url: Some(format!("fixture://paperclip/issues/{n}")) })
            .clone())
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
