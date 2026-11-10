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
}
