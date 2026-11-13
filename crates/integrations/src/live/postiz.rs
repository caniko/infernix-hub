//! Postiz Publisher adapter.
//!
//! Postiz is **not deployed** in this fleet; this adapter is built from the
//! public API docs (https://docs.postiz.com/public-api, fetched 2026-11-09) and
//! is marked "built, not contract-verified" in docs/capabilities.md.
//!
//! Contract points used:
//! * `POST {base}/posts` with `type: "now"` publishes immediately. The hub never
//!   schedules inside Postiz: delayed delivery belongs to the hub's own timer.
//! * `POST {base}/upload` (multipart field `file`) returns `{id, path}` for
//!   media referenced by a post's `image` array.
//! * `GET {base}/posts?startDate&endDate` lists posts but has **no**
//!   idempotency-key or custom-field filter, so `lookup` is `Unsupported`: an
//!   ambiguous submit must be reconciled by hand.
//! * There is no reply endpoint; replies/threads are separate posts in a group,
//!   so `reply` and `lookup_reply` are `Unsupported`.
//!
//! `Authorization` carries the API key verbatim (Postiz's documented scheme,
//! not a bearer token).

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use domain::ports::{MediaBlob, PortError, PortResult, ProviderPost, ProviderState, PublishRequest, Publisher, ReplyRequest};
use serde_json::{Value, json};

use super::{read_json, transport_error, trim_base};

const BOUNDARY: &str = "fleetix-hub-postiz-boundary-9f2c4e7a";

/// Postiz connection settings. `Debug` is manual so the API key cannot leak.
#[derive(Clone)]
pub struct PostizConfig {
    /// Public API base, e.g. `https://api.postiz.com/public/v1` or a
    /// self-hosted `https://host/api/public/v1`.
    pub base_url: String,
    /// Postiz API key (`Authorization: <key>`).
    pub api_key: String,
}

impl PostizConfig {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self { base_url: trim_base(&base_url.into()), api_key: api_key.into() }
    }
}

impl std::fmt::Debug for PostizConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostizConfig").field("base_url", &self.base_url).field("api_key", &"<redacted>").finish()
    }
}

pub struct PostizClient {
    http: reqwest::Client,
    config: PostizConfig,
}

impl PostizClient {
    pub fn new(http: reqwest::Client, config: PostizConfig) -> Self {
        Self { http, config }
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header(reqwest::header::AUTHORIZATION, &self.config.api_key)
    }

    /// Upload one media blob and return the `{id, path}` entry Postiz expects in
    /// a post's `image` array. Built by hand because the workspace `reqwest`
    /// has no `multipart` feature and this crate may not add one.
    async fn upload(&self, blob: &MediaBlob) -> PortResult<Value> {
        let filename = format!("{}.{}", blob.sha256, extension_for(&blob.mime));
        let body = multipart_body(blob, &filename);
        let resp = self
            .auth(self.http.post(format!("{}/upload", self.config.base_url)))
            .header(reqwest::header::CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(reqwest::Body::from(body))
            .send()
            .await
            .map_err(|e| transport_error(e, true))?;
        let value = read_json(resp, false, true).await?;
        let path =
            value.get("path").and_then(Value::as_str).ok_or_else(|| PortError::Ambiguous("postiz upload response has no path".into()))?;
        let id = value.get("id").and_then(Value::as_str).unwrap_or(&blob.sha256);
        Ok(json!({ "id": id, "path": path }))
    }
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        _ => "bin",
    }
}

fn multipart_body(blob: &MediaBlob, filename: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(blob.bytes.len() + 512);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
            blob.mime
        )
        .as_bytes(),
    );
    body.extend_from_slice(&blob.bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"adjust\"\r\n\r\nfalse\r\n--{BOUNDARY}--\r\n").as_bytes(),
    );
    body
}

/// The immediate-post payload. `date` is required by the API even for
/// `type: "now"`, so it carries the hub's submit time.
fn post_body(req: &PublishRequest, images: Vec<Value>, now: chrono::DateTime<Utc>) -> PortResult<Value> {
    if req.reply_to.is_some() {
        return Err(PortError::Unsupported("postiz has no reply endpoint; reply_to cannot be honoured".into()));
    }
    Ok(json!({
        "type": "now",
        "date": now.to_rfc3339_opts(SecondsFormat::Millis, true),
        "shortLink": false,
        "tags": [],
        "posts": [{
            "integration": { "id": req.account_id },
            "value": [{ "content": req.text, "image": images }],
            "settings": req.platform_settings,
        }],
    }))
}

fn provider_post(value: &Value) -> PortResult<ProviderPost> {
    let provider_id =
        value.get("postId").and_then(Value::as_str).ok_or_else(|| PortError::Ambiguous("postiz create response has no postId".into()))?;
    Ok(ProviderPost {
        provider_id: provider_id.to_string(),
        // The transport accepted the immediate post; confirmation on the
        // platform arrives later through Postiz, not in this response.
        state: ProviderState::Accepted,
        url: None,
        raw: value.clone(),
    })
}

#[async_trait]
impl Publisher for PostizClient {
    async fn submit(&self, req: &PublishRequest) -> PortResult<ProviderPost> {
        if req.reply_to.is_some() {
            return Err(PortError::Unsupported("postiz has no reply endpoint; reply_to cannot be honoured".into()));
        }
        // The documented upload/post payloads carry no alt text; publishing the
        // approved media without it would change the approved revision.
        if req.media.iter().any(|m| !m.alt_text.trim().is_empty()) {
            return Err(PortError::Unsupported("postiz public API has no documented media alt text; refusing to drop it".into()));
        }
        let mut images = Vec::with_capacity(req.media.len());
        for blob in &req.media {
            images.push(self.upload(blob).await?);
        }
        let body = post_body(req, images, Utc::now())?;
        let resp = self
            .auth(self.http.post(format!("{}/posts", self.config.base_url)))
            .json(&body)
            .send()
            .await
            .map_err(|e| transport_error(e, true))?;
        let value = read_json(resp, false, true).await?;
        let first = value
            .as_array()
            .and_then(|posts| posts.first())
            .ok_or_else(|| PortError::Ambiguous("postiz create response was empty".into()))?;
        provider_post(first)
    }

    async fn lookup(&self, _account_id: &str, _idempotency_key: &str) -> PortResult<Option<ProviderPost>> {
        Err(PortError::Unsupported(
            "postiz public API has no idempotency key or custom-field filter on GET /posts; \
             reconcile ambiguous submits manually"
                .into(),
        ))
    }

    async fn reply(&self, _req: &ReplyRequest) -> PortResult<ProviderPost> {
        Err(PortError::Unsupported(
            "postiz public API has no reply endpoint; model threads as grouped posts when the contract is verified".into(),
        ))
    }

    async fn lookup_reply(&self, _account_id: &str, _idempotency_key: &str) -> PortResult<Option<ProviderPost>> {
        Err(PortError::Unsupported("postiz public API has no reply lookup endpoint".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::ports::MediaBlob;

    fn config() -> PostizConfig {
        PostizConfig::new("https://api.postiz.com/public/v1/", "postiz-secret-key")
    }

    fn request() -> PublishRequest {
        PublishRequest {
            account_id: "integration-1".into(),
            platform: "x".into(),
            idempotency_key: "idem-1".into(),
            text: "hello".into(),
            media: vec![],
            platform_settings: json!({ "__type": "x", "who_can_reply_post": "everyone" }),
            reply_to: None,
        }
    }

    #[test]
    fn debug_redacts_the_api_key() {
        let dbg = format!("{:?}", config());
        assert!(!dbg.contains("postiz-secret-key"), "secret leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn immediate_posts_are_never_scheduled() {
        let body = post_body(&request(), vec![json!({"id": "m1", "path": "https://uploads/x.png"})], Utc::now()).unwrap();
        assert_eq!(body["type"], "now");
        assert!(body["date"].as_str().unwrap().contains('T'));
        assert_eq!(body["posts"][0]["integration"]["id"], "integration-1");
        assert_eq!(body["posts"][0]["value"][0]["content"], "hello");
        assert_eq!(body["posts"][0]["value"][0]["image"][0]["path"], "https://uploads/x.png");
        assert_eq!(body["posts"][0]["settings"]["__type"], "x");
    }

    #[test]
    fn reply_to_is_rejected_before_any_request() {
        let mut req = request();
        req.reply_to = Some("post-1".into());
        assert!(matches!(post_body(&req, vec![], Utc::now()), Err(PortError::Unsupported(_))));
    }

    #[test]
    fn multipart_body_carries_file_bytes_and_metadata() {
        let blob =
            MediaBlob { sha256: "abc123".into(), mime: "image/png".into(), alt_text: "alt".into(), bytes: vec![0x89, b'P', b'N', b'G'] };
        let body = multipart_body(&blob, "abc123.png");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"file\"; filename=\"abc123.png\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(text.contains("name=\"adjust\""));
        assert!(body.windows(4).any(|window| window == [0x89, b'P', b'N', b'G']));
        assert_eq!(extension_for("image/jpeg"), "jpg");
        assert_eq!(extension_for("application/octet-stream"), "bin");
    }

    #[test]
    fn response_maps_to_an_accepted_provider_post() {
        let post = provider_post(&json!({"postId": "post-123", "integration": "integration-1"})).unwrap();
        assert_eq!(post.provider_id, "post-123");
        assert_eq!(post.state, ProviderState::Accepted);
        assert!(post.url.is_none());
    }
}
