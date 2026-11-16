//! Typed HTTP client for the hub API, shared by the desktop UI and the CLI so
//! both go through the same server-side authorization and command handling.

use crate::*;
use serde::de::DeserializeOwned;

#[derive(Debug)]
pub enum ClientError {
    Transport(String),
    Api { status: u16, error: ApiError },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Transport(e) => write!(f, "hub unreachable: {e}"),
            ClientError::Api { status, error } => {
                write!(f, "{} ({status}): {}", error.code, error.message)
            }
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Clone)]
pub struct HubClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl std::fmt::Debug for HubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubClient").field("base", &self.base).field("token", &"<redacted>").finish()
    }
}

impl HubClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self { http: reqwest::Client::new(), base, token: token.into() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{API_VERSION}{path}", self.base)
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T, ClientError> {
        let resp = req
            .bearer_auth(&self.token)
            .header(CORRELATION_HEADER, uuid::Uuid::new_v4().to_string())
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            let error = serde_json::from_slice(&body).unwrap_or_else(|_| ApiError {
                code: "http_error".into(),
                message: String::from_utf8_lossy(&body).into_owned(),
                correlation_id: None,
            });
            return Err(ClientError::Api { status: status.as_u16(), error });
        }
        let body: &[u8] = if body.is_empty() { b"null" } else { &body };
        serde_json::from_slice(body).map_err(|e| ClientError::Transport(format!("decode: {e}")))
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send(self.http.get(self.url(path))).await
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, ClientError> {
        self.send(self.http.post(self.url(path)).json(body)).await
    }

    pub async fn put<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, ClientError> {
        self.send(self.http.put(self.url(path)).json(body)).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send(self.http.delete(self.url(path))).await
    }

    /// Raw bytes (asset content) — returns `(mime, bytes)`.
    pub async fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>), ClientError> {
        let resp =
            self.http.get(self.url(path)).bearer_auth(&self.token).send().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ClientError::Api {
                status: status.as_u16(),
                error: ApiError { code: "http_error".into(), message: String::from_utf8_lossy(&bytes).into(), correlation_id: None },
            });
        }
        Ok((mime, bytes.to_vec()))
    }

    // Typed helpers for the main journey.
    pub async fn status(&self) -> Result<SystemStatus, ClientError> {
        self.get("/status").await
    }
    /// The fleet operations snapshot; read-only, produced by another tool.
    pub async fn fleet_status(&self) -> Result<fleet::FleetStatus, ClientError> {
        self.get("/fleet/status").await
    }
    pub async fn today(&self) -> Result<Today, ClientError> {
        self.get("/today").await
    }
    pub async fn news(&self) -> Result<Vec<NewsItem>, ClientError> {
        self.get("/news").await
    }
    pub async fn personas(&self) -> Result<Vec<Persona>, ClientError> {
        self.get("/personas").await
    }
    pub async fn influencers(&self) -> Result<Vec<Influencer>, ClientError> {
        self.get("/influencers").await
    }
    pub async fn influencer(&self, id: Uuid) -> Result<Influencer, ClientError> {
        self.get(&format!("/influencers/{id}")).await
    }
    pub async fn create_influencer(&self, body: &InfluencerIn) -> Result<Influencer, ClientError> {
        self.post("/influencers", body).await
    }
    pub async fn update_influencer(&self, id: Uuid, body: &InfluencerIn) -> Result<Influencer, ClientError> {
        self.put(&format!("/influencers/{id}"), body).await
    }
    pub async fn put_influencer_member(&self, id: Uuid, persona: Uuid, body: &InfluencerMemberIn) -> Result<Influencer, ClientError> {
        self.put(&format!("/influencers/{id}/members/{persona}"), body).await
    }
    pub async fn remove_influencer_member(&self, id: Uuid, persona: Uuid) -> Result<Influencer, ClientError> {
        self.delete(&format!("/influencers/{id}/members/{persona}")).await
    }
    pub async fn set_influencer_accounts(&self, id: Uuid, body: &InfluencerAccountsIn) -> Result<Influencer, ClientError> {
        self.put(&format!("/influencers/{id}/accounts"), body).await
    }
    pub async fn workflows(&self) -> Result<Vec<Workflow>, ClientError> {
        self.get("/workflows").await
    }
    pub async fn accounts(&self) -> Result<Vec<Account>, ClientError> {
        self.get("/accounts").await
    }
    pub async fn content(&self) -> Result<Vec<ContentItem>, ClientError> {
        self.get("/content").await
    }
    pub async fn content_detail(&self, id: Uuid) -> Result<ContentDetail, ClientError> {
        self.get(&format!("/content/{id}")).await
    }
    pub async fn create_brief(&self, body: &CreateBriefIn) -> Result<ContentItem, ClientError> {
        self.post("/content", body).await
    }
    pub async fn generate(&self, content: Uuid, body: &GenerateIn) -> Result<GenerationRequest, ClientError> {
        self.post(&format!("/content/{content}/generations"), body).await
    }
    pub async fn cancel_generation(&self, id: Uuid) -> Result<CancelOut, ClientError> {
        self.post(&format!("/generations/{id}/cancel"), &()).await
    }
    pub async fn select_asset(&self, id: Uuid, body: &SelectAssetIn) -> Result<Asset, ClientError> {
        self.post(&format!("/assets/{id}/select"), body).await
    }
    pub async fn freeze_release(&self, content: Uuid, body: &FreezeReleaseIn) -> Result<ReleaseRevision, ClientError> {
        self.post(&format!("/content/{content}/releases"), body).await
    }
    pub async fn request_review(&self, release: Uuid) -> Result<ReleaseRevision, ClientError> {
        self.post(&format!("/releases/{release}/request-review"), &()).await
    }
    pub async fn schedule(&self, release: Uuid, body: &ScheduleIn) -> Result<Vec<PublicationIntent>, ClientError> {
        self.post(&format!("/releases/{release}/schedule"), body).await
    }
    pub async fn cancel_intent(&self, intent: Uuid) -> Result<PublicationIntent, ClientError> {
        self.post(&format!("/intents/{intent}/cancel"), &()).await
    }
    pub async fn intents(&self) -> Result<Vec<PublicationIntent>, ClientError> {
        self.get("/intents").await
    }
    pub async fn relationships(&self) -> Result<Vec<Relationship>, ClientError> {
        self.get("/relationships").await
    }
    pub async fn relationship(&self, id: Uuid) -> Result<RelationshipDetail, ClientError> {
        self.get(&format!("/relationships/{id}")).await
    }
    pub async fn draft_reply(&self, rel: Uuid, body: &DraftReplyIn) -> Result<ReplyDraft, ClientError> {
        self.post(&format!("/relationships/{rel}/drafts"), body).await
    }
    /// Ask the assistant for a reply draft (operator only).
    pub async fn draft_assistant_reply(&self, rel: Uuid) -> Result<ReplyDraft, ClientError> {
        self.post(&format!("/relationships/{rel}/draft-reply"), &()).await
    }
    pub async fn decide_draft(&self, draft: Uuid, body: &DraftDecisionIn) -> Result<ReplyDraft, ClientError> {
        self.post(&format!("/drafts/{draft}/decision"), body).await
    }
    pub async fn takeover(&self, rel: Uuid) -> Result<Relationship, ClientError> {
        self.post(&format!("/relationships/{rel}/takeover"), &()).await
    }
    pub async fn release_to_agent(&self, rel: Uuid) -> Result<Relationship, ClientError> {
        self.post(&format!("/relationships/{rel}/release"), &()).await
    }
    pub async fn handoff(&self, rel: Uuid) -> Result<HandoffOut, ClientError> {
        self.post(&format!("/relationships/{rel}/handoff"), &()).await
    }
    pub async fn chat(&self, subject_type: &str, id: Uuid) -> Result<Vec<ChatMessage>, ClientError> {
        self.get(&format!("/chat/{subject_type}/{id}")).await
    }
    pub async fn send_chat(&self, subject_type: &str, id: Uuid, body: &ChatMessageIn) -> Result<Vec<ChatMessage>, ClientError> {
        self.post(&format!("/chat/{subject_type}/{id}"), body).await
    }
    pub async fn canon(&self, persona: Uuid) -> Result<Vec<CanonRevision>, ClientError> {
        self.get(&format!("/personas/{persona}/canon")).await
    }
    pub async fn set_profile(&self, persona: Uuid, body: &CharacterProfile) -> Result<Persona, ClientError> {
        self.put(&format!("/personas/{persona}/profile"), body).await
    }
    pub async fn draft_canon(&self, persona: Uuid, body: &CanonDraftIn) -> Result<CanonBody, ClientError> {
        self.post(&format!("/personas/{persona}/canon-draft"), body).await
    }
    pub async fn request_canon_comments(&self, revision: Uuid) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/canon/{revision}/request-comments"), &()).await
    }
    pub async fn comment_canon(&self, revision: Uuid, body: &CanonCommentIn) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/canon/{revision}/comments"), body).await
    }
    pub async fn canon_reviews(&self) -> Result<Vec<CanonRevision>, ClientError> {
        self.get("/canon/reviews").await
    }
    pub async fn propose_canon(&self, persona: Uuid, body: &CanonProposalIn) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/personas/{persona}/canon"), body).await
    }
    pub async fn decide_canon(&self, revision: Uuid, body: &CanonDecisionIn) -> Result<CanonRevision, ClientError> {
        self.post(&format!("/canon/{revision}/decision"), body).await
    }
    pub async fn search(&self, q: &str) -> Result<Vec<SearchHit>, ClientError> {
        self.send(self.http.get(self.url("/search")).query(&[("q", q)])).await
    }
    pub async fn assets(&self) -> Result<Vec<Asset>, ClientError> {
        self.get("/assets").await
    }
    pub async fn set_pause(&self, body: &PauseIn) -> Result<SystemStatus, ClientError> {
        self.post("/system/pause", body).await
    }
    // ── Opportunities (H1) ──────────────────────────────────────────────
    pub async fn opportunities(&self, state: Option<&str>, due_within_days: Option<i32>) -> Result<Vec<Opportunity>, ClientError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(state) = state {
            query.push(("state", state.to_string()));
        }
        if let Some(days) = due_within_days {
            query.push(("due_within_days", days.to_string()));
        }
        self.send(self.http.get(self.url("/opportunities")).query(&query)).await
    }
    pub async fn opportunity(&self, id: Uuid) -> Result<Opportunity, ClientError> {
        self.get(&format!("/opportunities/{id}")).await
    }
    pub async fn ingest_opportunity(&self, body: &OpportunityIn) -> Result<Opportunity, ClientError> {
        self.post("/ingest/opportunities", body).await
    }
    pub async fn decide_opportunity(&self, id: Uuid, body: &OpportunityDecisionIn) -> Result<Opportunity, ClientError> {
        self.post(&format!("/opportunities/{id}/decision"), body).await
    }
    // ── Renewals (H3) ───────────────────────────────────────────────────
    pub async fn renewals(&self) -> Result<Vec<Renewal>, ClientError> {
        self.get("/renewals").await
    }
    /// Operator-only: record a renewal; `due_at` wins over the stored cadence.
    pub async fn renewal_renewed(&self, key: &str, body: &RenewalRenewedIn) -> Result<Renewal, ClientError> {
        self.post(&format!("/renewals/{key}/renewed"), body).await
    }
    /// Operator or procedure: record one provider answer. Never a rotation.
    pub async fn observe_renewal(&self, key: &str, body: &ObserveRenewalIn) -> Result<Renewal, ClientError> {
        self.post(&format!("/renewals/{key}/observe"), body).await
    }
    // ── Notifications ───────────────────────────────────────────────────
    /// Rows newer than `after`; `wait` seconds holds the request open on the
    /// server until something changes. `all` includes acknowledged and resolved
    /// rows.
    /// `after: None` reads a snapshot of the current set; `Some(cursor)` reads
    /// the feed of changes past `cursor` (echo the returned `cursor` back).
    pub async fn notifications(&self, after: Option<i64>, wait: u64, all: bool) -> Result<NotificationPage, ClientError> {
        let mut query: Vec<(&str, String)> = vec![("state", if all { "all".into() } else { "open".into() })];
        if let Some(after) = after {
            query.push(("after", after.to_string()));
        }
        if wait > 0 {
            query.push(("wait", wait.to_string()));
        }
        self.send(self.http.get(self.url("/notifications")).query(&query)).await
    }
    pub async fn raise_notification(&self, body: &RaiseNotificationIn) -> Result<Notification, ClientError> {
        self.post("/notifications", body).await
    }
    pub async fn sync_notifications(&self, body: &SyncNotificationsIn) -> Result<SyncNotificationsOut, ClientError> {
        self.post("/notifications/sync", body).await
    }
    /// Acknowledge `ids` or `all`; returns the remaining open page.
    pub async fn ack_notifications(&self, body: &AckNotificationsIn) -> Result<NotificationPage, ClientError> {
        self.post("/notifications/ack", body).await
    }
    /// Hide one row until `until`.
    pub async fn snooze_notification(&self, id: Uuid, body: &SnoozeNotificationIn) -> Result<Notification, ClientError> {
        self.post(&format!("/notifications/{id}/snooze"), body).await
    }
    // ── Release approval (H2) ───────────────────────────────────────────
    pub async fn release(&self, id: Uuid) -> Result<ReleaseRevision, ClientError> {
        self.get(&format!("/releases/{id}")).await
    }
    /// Re-read the native approval and bind it to this exact revision.
    pub async fn link_approval(&self, release: Uuid, body: &LinkApprovalIn) -> Result<ReleaseRevision, ClientError> {
        self.post(&format!("/releases/{release}/link-approval"), body).await
    }
}

/// The operator's own Paperclip board client.
///
/// H2 adds no privilege to the hub: the operator's key is read on their
/// machine and used here, and only the resulting native decision is linked
/// through [`HubClient::link_approval`]. `Debug` is manual so the key cannot
/// leak through logs.
#[derive(Clone)]
pub struct PaperclipBoard {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl std::fmt::Debug for PaperclipBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaperclipBoard").field("base", &self.base).field("key", &"<redacted>").finish()
    }
}

#[derive(Debug)]
pub enum PaperclipError {
    Config(String),
    Transport(String),
    Api { status: u16, message: String },
}

impl std::fmt::Display for PaperclipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaperclipError::Config(message) => write!(f, "paperclip config: {message}"),
            PaperclipError::Transport(message) => write!(f, "paperclip unreachable: {message}"),
            PaperclipError::Api { status, message } => write!(f, "paperclip {status}: {message}"),
        }
    }
}

impl std::error::Error for PaperclipError {}

impl PaperclipBoard {
    pub fn new(base: impl Into<String>, key: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self { http: reqwest::Client::new(), base, key: key.into() }
    }

    /// The key file and URL use the Paperclip CLI's own variables,
    /// `PAPERCLIP_API_KEY_FILE` and `PAPERCLIP_API_URL`. Without the first,
    /// the key is read from `$XDG_CONFIG_HOME/paperclip/api-key` (or
    /// `~/.config/paperclip/api-key`).
    pub fn from_env() -> Result<Self, PaperclipError> {
        let path = match std::env::var_os("PAPERCLIP_API_KEY_FILE").filter(|value| !value.is_empty()) {
            Some(file) => std::path::PathBuf::from(file),
            None => std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home| std::path::PathBuf::from(home).join(".config"))
                })
                .ok_or_else(|| PaperclipError::Config("set PAPERCLIP_API_KEY_FILE, XDG_CONFIG_HOME or HOME".into()))?
                .join("paperclip")
                .join("api-key"),
        };
        let key = std::fs::read_to_string(&path).map_err(|e| PaperclipError::Config(format!("reading {}: {e}", path.display())))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(PaperclipError::Config(format!("{} is empty", path.display())));
        }
        Ok(Self::new(std::env::var("PAPERCLIP_API_URL").unwrap_or_else(|_| "http://127.0.0.1:3117".into()), key))
    }

    /// Approve or reject one native approval as the board user. The body is
    /// `resolveApprovalSchema` (`{ decisionNote?: string }`) and the note is
    /// omitted when absent.
    pub async fn decide(&self, approval_id: &str, approve: bool, note: Option<&str>) -> Result<serde_json::Value, PaperclipError> {
        // The id is a path segment; anything else could change the request path.
        if approval_id.is_empty() || !approval_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(PaperclipError::Config(format!("approval id {approval_id:?} is not a safe path segment")));
        }
        let action = if approve { "approve" } else { "reject" };
        let mut body = serde_json::Map::new();
        if let Some(note) = note {
            body.insert("decisionNote".into(), serde_json::Value::String(note.to_string()));
        }
        let resp = self
            .http
            .post(format!("{}/api/approvals/{approval_id}/{action}", self.base))
            .bearer_auth(&self.key)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| PaperclipError::Transport(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| PaperclipError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(PaperclipError::Api { status: status.as_u16(), message: String::from_utf8_lossy(&bytes).into_owned() });
        }
        serde_json::from_slice(&bytes).map_err(|e| PaperclipError::Transport(format!("decode: {e}")))
    }
}
