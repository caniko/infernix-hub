//! Fixture publishing transport: deterministic posts, replies and the failure
//! modes the guarded dispatch path must classify.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use domain::ports::*;
use serde_json::json;

use super::lock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    Publish,
    /// Accepted now, confirmed published on the next lookup.
    AcceptThenPublish,
    Reject,
    /// Records the post but the response is lost.
    TimeoutAfterSend,
    /// The claim row exists and the platform call never produced a result:
    /// submit and lookup both stay ambiguous until an operator resolves it.
    PendingClaim,
    Unreachable,
}

/// What the transport holds for one idempotency key. A pending claim is a row
/// without a result, never a provider post.
#[derive(Clone)]
enum Stored {
    Pending,
    Post(ProviderPost),
}

#[derive(Default)]
pub struct FixturePublisher {
    posts: Mutex<HashMap<String, Stored>>,
    replies: Mutex<HashMap<String, ProviderPost>>,
    modes: Mutex<HashMap<String, PublishMode>>,
    submissions: Mutex<Vec<PublishRequest>>,
    reply_submissions: Mutex<Vec<ReplyRequest>>,
}

impl FixturePublisher {
    pub fn set_mode(&self, account: &str, mode: PublishMode) {
        lock(&self.modes).insert(account.into(), mode);
    }

    fn mode(&self, account: &str) -> PublishMode {
        lock(&self.modes).get(account).copied().unwrap_or(PublishMode::Publish)
    }

    pub fn submissions(&self) -> Vec<PublishRequest> {
        lock(&self.submissions).clone()
    }

    pub fn reply_submissions(&self) -> Vec<ReplyRequest> {
        lock(&self.reply_submissions).clone()
    }
}

fn post(id: String, state: ProviderState) -> ProviderPost {
    ProviderPost {
        url: Some(format!("fixture://platform/posts/{id}")),
        raw: json!({ "fixture": true, "id": id, "state": state }),
        provider_id: id,
        state,
    }
}

#[async_trait]
impl Publisher for FixturePublisher {
    async fn submit(&self, req: &PublishRequest) -> PortResult<ProviderPost> {
        let mode = self.mode(&req.account_id);
        if mode == PublishMode::Unreachable {
            return Err(PortError::Unreachable("fixture transport offline".into()));
        }
        lock(&self.submissions).push(req.clone());
        let id = format!("fixture-post-{}", req.idempotency_key);
        // Claim-first, as the contract is: a key that is already claimed returns
        // the stored result and never reaches the platform again. The lookup
        // and the insert share one guard, so two concurrent submits of the same
        // key cannot both win the claim.
        let mut posts = lock(&self.posts);
        if let Some(stored) = posts.get(&req.idempotency_key).cloned() {
            return match stored {
                Stored::Post(p) => Ok(p),
                Stored::Pending => {
                    Err(PortError::Ambiguous(format!("fixture claim {} is pending (no platform result)", req.idempotency_key)))
                }
            };
        }
        match mode {
            PublishMode::Reject => {
                // A definite rejection is recorded as `failed`, so a replay
                // returns the stored failure instead of posting again.
                posts.insert(req.idempotency_key.clone(), Stored::Post(post(id, ProviderState::Failed)));
                Err(PortError::Rejected { status: 422, message: "fixture platform rejected the post".into() })
            }
            PublishMode::PendingClaim => {
                posts.insert(req.idempotency_key.clone(), Stored::Pending);
                Err(PortError::Ambiguous("fixture claim has no platform result yet".into()))
            }
            PublishMode::TimeoutAfterSend => {
                posts.insert(req.idempotency_key.clone(), Stored::Post(post(id, ProviderState::Published)));
                Err(PortError::Ambiguous("fixture timeout after send".into()))
            }
            PublishMode::AcceptThenPublish => {
                let p = post(id, ProviderState::Accepted);
                posts.insert(req.idempotency_key.clone(), Stored::Post(p.clone()));
                Ok(p)
            }
            _ => {
                let p = post(id, ProviderState::Published);
                posts.insert(req.idempotency_key.clone(), Stored::Post(p.clone()));
                Ok(p)
            }
        }
    }

    async fn lookup(&self, _account_id: &str, key: &str) -> PortResult<Option<ProviderPost>> {
        let mut posts = lock(&self.posts);
        let Some(stored) = posts.get_mut(key) else { return Ok(None) };
        match stored {
            // A claim with no result is uncertain, never a post and never a
            // definite failure.
            Stored::Pending => Err(PortError::Ambiguous(format!("fixture claim {key} is pending (no platform result)"))),
            Stored::Post(p) => {
                if p.state == ProviderState::Accepted {
                    p.state = ProviderState::Published;
                }
                Ok(Some(p.clone()))
            }
        }
    }

    async fn reply(&self, req: &ReplyRequest) -> PortResult<ProviderPost> {
        let mode = self.mode(&req.account_id);
        if mode == PublishMode::Unreachable {
            return Err(PortError::Unreachable("fixture transport offline".into()));
        }
        lock(&self.reply_submissions).push(req.clone());
        let p = post(format!("fixture-reply-{}", req.idempotency_key), ProviderState::Published);
        lock(&self.replies).insert(req.idempotency_key.clone(), p.clone());
        if mode == PublishMode::TimeoutAfterSend {
            return Err(PortError::Ambiguous("fixture timeout after send".into()));
        }
        Ok(p)
    }

    async fn lookup_reply(&self, _account_id: &str, key: &str) -> PortResult<Option<ProviderPost>> {
        Ok(lock(&self.replies).get(key).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ports::MediaBlob;

    fn request() -> PublishRequest {
        PublishRequest {
            account_id: "fixture-bluesky".into(),
            platform: "bluesky".into(),
            idempotency_key: "release-attempt-1".into(),
            text: "verified post".into(),
            media: vec![MediaBlob { sha256: "aa".repeat(32), mime: "image/png".into(), alt_text: String::new(), bytes: vec![1, 2, 3] }],
            platform_settings: json!({ "langs": ["en"] }),
            reply_to: None,
        }
    }

    #[tokio::test]
    async fn replay_returns_the_first_result_without_posting_again() {
        let fx = FixturePublisher::default();
        let req = request();
        let first = fx.submit(&req).await.unwrap();
        // The second attempt loses the claim, whatever the platform would do now.
        fx.set_mode(&req.account_id, PublishMode::Reject);
        let replay = fx.submit(&req).await.unwrap();
        assert_eq!(replay.provider_id, first.provider_id);
        assert_eq!(replay.state, ProviderState::Published);
        assert_eq!(fx.submissions().len(), 2);
    }

    #[tokio::test]
    async fn a_pending_claim_is_ambiguous_on_submit_and_lookup() {
        let fx = FixturePublisher::default();
        let req = request();
        fx.set_mode(&req.account_id, PublishMode::PendingClaim);
        assert!(matches!(fx.submit(&req).await, Err(PortError::Ambiguous(_))), "a pending claim is not a post");
        assert!(
            matches!(fx.lookup(&req.account_id, &req.idempotency_key).await, Err(PortError::Ambiguous(_))),
            "pending lookup must stay ambiguous: not published, not failed, not absent"
        );
        assert!(matches!(fx.submit(&req).await, Err(PortError::Ambiguous(_))), "a replay of a pending claim stays ambiguous");
        assert_eq!(fx.submissions().len(), 2);
    }

    #[tokio::test]
    async fn a_rejection_is_recorded_and_replayed_as_a_failure() {
        let fx = FixturePublisher::default();
        let req = request();
        fx.set_mode(&req.account_id, PublishMode::Reject);
        assert!(matches!(fx.submit(&req).await, Err(PortError::Rejected { status: 422, .. })));
        let stored = fx.lookup(&req.account_id, &req.idempotency_key).await.unwrap().expect("a definite rejection is recorded");
        assert_eq!(stored.state, ProviderState::Failed);
        assert!(matches!(fx.submit(&req).await, Ok(p) if p.state == ProviderState::Failed), "a replay returns the stored failure");
    }

    #[tokio::test]
    async fn an_unknown_key_is_absent_and_accepted_confirms_on_lookup() {
        let fx = FixturePublisher::default();
        let req = request();
        assert!(fx.lookup(&req.account_id, &req.idempotency_key).await.unwrap().is_none());
        fx.set_mode(&req.account_id, PublishMode::AcceptThenPublish);
        assert_eq!(fx.submit(&req).await.unwrap().state, ProviderState::Accepted);
        assert_eq!(fx.lookup(&req.account_id, &req.idempotency_key).await.unwrap().unwrap().state, ProviderState::Published);
    }

    /// The claim check and the claim insert are one critical section. A second
    /// submit may read the winner's stored result, but it must never reach the
    /// platform: `Reject` makes a second winner observable as a second
    /// `Rejected` instead of a stored `Failed`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_submits_of_one_key_claim_exactly_once() {
        use std::sync::Arc;

        let fx = Arc::new(FixturePublisher::default());
        let req = request();
        fx.set_mode(&req.account_id, PublishMode::Reject);
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let fx = fx.clone();
            let req = req.clone();
            tasks.push(tokio::spawn(async move { fx.submit(&req).await }));
        }
        let mut rejected = 0;
        let mut stored = 0;
        for task in tasks {
            match task.await.unwrap() {
                Err(PortError::Rejected { status: 422, .. }) => rejected += 1,
                Ok(post) if post.state == ProviderState::Failed => stored += 1,
                other => panic!("unexpected claim result: {other:?}"),
            }
        }
        assert_eq!(rejected, 1, "exactly one submit owns the claim");
        assert_eq!(stored, 1, "the loser reads the winner's stored result");
        assert_eq!(fx.submissions().len(), 2, "both attempts were recorded");
    }
}
