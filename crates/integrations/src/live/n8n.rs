//! n8n adapter, adapted from the MIT-licensed donor
//! `aivyx-n8n/src/n8n_client.rs` (provenance and sha256 in
//! docs/provenance-integrations.md).
//!
//! Deployment facts (n8n 2.39.6, `http://127.0.0.1:5680`): the public REST API
//! `/api/v1/*` needs `X-N8N-API-KEY`, and **no such key is provisioned**. Only
//! a webhook header token exists (`X-Studio-Token`). Therefore:
//! * [`N8nClient::trigger_webhook`] is the supported production path
//!   (`POST /webhook/<path>` with `X-Studio-Token`);
//! * the REST methods are kept from the donor but are **unverified** against
//!   this deployment and return [`N8nClientError::InvalidInput`] when no API key
//!   is configured.
//!
//! Modifications from the donor: secrets are wrapped in [`SecretString`] and
//! the config's `Debug` is manual, both redacting; a webhook trigger was added;
//! `N8nClientError::Ambiguous` was added so a webhook whose body was already
//! sent is never blindly retried; the TOML test was dropped (no `toml` dep).
//!
//! Contract-v1 lookup mapping ([`Publisher::lookup`]) — the only way to resolve
//! a claim, so an answer without a state is never a definite outcome:
//!
//! | Lookup answer | `PortError` | Why |
//! | --- | --- | --- |
//! | HTTP 404 | `Unsupported` | the platform workflow (and its lookup path) is not deployed; nothing can be read |
//! | connect refused | `Unreachable` | no read happened; safe to try again |
//! | timeout/reset after send, any other non-2xx, malformed JSON | `Ambiguous` | the claim may exist and its state is unknown |
//! | `absent` | `Ok(None)` | the provider has no record |
//! | `pending` | `Ambiguous` | a claim with no platform result |
//!
//! Hub reconciliation moves `submitting` to `outcome_unknown` on `Unsupported`
//! and `Ambiguous`, under the row lock, for an operator to resolve.

use std::sync::Arc;

use async_trait::async_trait;
use domain::ports::{PortError, PortResult, ProviderPost, ProviderState, PublishRequest, Publisher, ReplyRequest};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use super::{read_json, snippet, transport_error};

/// API path prefix relative to the operator-supplied base URL. n8n's REST API
/// lives at `{base_url}/api/v1/<resource>`.
pub const N8N_API_PREFIX: &str = "/api/v1";

/// HTTP header n8n uses for API-key auth.
pub const N8N_API_KEY_HEADER: &str = "X-N8N-API-KEY";

/// Header the deployed `studio-webhook` credential expects.
pub const N8N_WEBHOOK_TOKEN_HEADER: &str = "X-Studio-Token";

/// A credential that must never be printed. No `Display` impl exists on
/// purpose; `Debug` and `Display`-adjacent formatting both show `<redacted>`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The only accessor. Callers pass the value straight into a header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Error)]
pub enum N8nClientError {
    #[error("HTTP transport error: {0}")]
    Transport(String),
    /// The request may have reached n8n (timeout/reset after send). Do not
    /// resend a webhook; reconcile the workflow execution instead.
    #[error("n8n outcome unknown: {0}")]
    Ambiguous(String),
    #[error("n8n API returned HTTP {status}: {message}")]
    Api { status: u16, message: String },
    #[error("response parse error: {0}")]
    Parse(String),
    #[error("input validation: {0}")]
    InvalidInput(String),
    #[error("resource not found (HTTP 404)")]
    NotFound,
}

/// Operator-supplied configuration. `Debug` is manual so neither credential can
/// leak through `{:?}` or tracing fields.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct N8nConfig {
    /// Base URL of the n8n instance, no trailing slash.
    pub n8n_base_url: String,
    /// API key from n8n Settings → API. `None` when only webhooks are used.
    pub n8n_api_key: Option<SecretString>,
    /// Header token for `/webhook/<path>` (`X-Studio-Token`).
    pub webhook_token: Option<SecretString>,
}

impl N8nConfig {
    pub fn new(n8n_base_url: impl Into<String>, n8n_api_key: impl Into<String>) -> Self {
        Self { n8n_base_url: n8n_base_url.into(), n8n_api_key: Some(SecretString::new(n8n_api_key)), webhook_token: None }
    }

    /// Configuration for the deployed instance, which has a webhook token but
    /// no REST API key.
    pub fn webhook_only(n8n_base_url: impl Into<String>, webhook_token: impl Into<String>) -> Self {
        Self { n8n_base_url: n8n_base_url.into(), n8n_api_key: None, webhook_token: Some(SecretString::new(webhook_token)) }
    }

    pub fn with_webhook_token(mut self, webhook_token: impl Into<String>) -> Self {
        self.webhook_token = Some(SecretString::new(webhook_token));
        self
    }
}

impl std::fmt::Debug for N8nConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("N8nConfig")
            .field("n8n_base_url", &self.n8n_base_url)
            .field("n8n_api_key", &self.n8n_api_key.as_ref().map(|_| "<redacted>"))
            .field("webhook_token", &self.webhook_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

pub struct N8nClient {
    http: Client,
    config: N8nConfig,
}

impl N8nClient {
    pub fn new(http: Client, config: N8nConfig) -> Self {
        let mut config = config;
        while config.n8n_base_url.ends_with('/') {
            config.n8n_base_url.pop();
        }
        Self { http, config }
    }

    fn url_for(&self, path: &str) -> String {
        format!("{}{}{}", self.config.n8n_base_url, N8N_API_PREFIX, path)
    }

    /// Production webhook URL: `{base}/webhook/{path}` (never `/api/v1`).
    pub fn webhook_url(&self, path: &str) -> String {
        format!("{}/webhook/{}", self.config.n8n_base_url, path.trim_start_matches('/'))
    }

    fn api_key(&self) -> Result<&str, N8nClientError> {
        self.config
            .n8n_api_key
            .as_ref()
            .map(SecretString::expose)
            .ok_or_else(|| N8nClientError::InvalidInput("n8n REST API key is not configured; only the webhook token exists".into()))
    }

    /// GET `{base}/api/v1{path}` with the API-key header.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T, N8nClientError> {
        let resp = self
            .http
            .get(self.url_for(path))
            .header(N8N_API_KEY_HEADER, self.api_key()?)
            .query(query)
            .send()
            .await
            .map_err(|e| N8nClientError::Transport(e.to_string()))?;
        decode_response(resp).await
    }

    /// POST JSON body to the REST API.
    pub async fn post_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, N8nClientError> {
        let resp = self
            .http
            .post(self.url_for(path))
            .header(N8N_API_KEY_HEADER, self.api_key()?)
            .json(body)
            .send()
            .await
            .map_err(|e| N8nClientError::Transport(e.to_string()))?;
        decode_response(resp).await
    }

    /// PATCH JSON body.
    pub async fn patch_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, N8nClientError> {
        let resp = self
            .http
            .patch(self.url_for(path))
            .header(N8N_API_KEY_HEADER, self.api_key()?)
            .json(body)
            .send()
            .await
            .map_err(|e| N8nClientError::Transport(e.to_string()))?;
        decode_response(resp).await
    }

    /// PUT JSON body (n8n's update-workflow endpoint expects a full replacement).
    pub async fn put_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, N8nClientError> {
        let resp = self
            .http
            .put(self.url_for(path))
            .header(N8N_API_KEY_HEADER, self.api_key()?)
            .json(body)
            .send()
            .await
            .map_err(|e| N8nClientError::Transport(e.to_string()))?;
        decode_response(resp).await
    }

    /// DELETE returning the raw status so the caller can treat 404 as
    /// "already deleted".
    pub async fn delete(&self, path: &str) -> Result<StatusCode, N8nClientError> {
        let resp = self
            .http
            .delete(self.url_for(path))
            .header(N8N_API_KEY_HEADER, self.api_key()?)
            .send()
            .await
            .map_err(|e| N8nClientError::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(status)
        } else {
            let body = resp.text().await.unwrap_or_else(|_| "<no body>".to_string());
            Err(N8nClientError::Api { status: status.as_u16(), message: body })
        }
    }

    /// Trigger a production webhook (`POST /webhook/<path>`) with the
    /// `X-Studio-Token` header. Returns the parsed JSON body, or `Null` when the
    /// workflow answered with an empty body.
    ///
    /// A transport failure after the body was sent is
    /// [`N8nClientError::Ambiguous`]: the workflow may already be running.
    pub async fn trigger_webhook(&self, path: &str, body: &Value) -> Result<Value, N8nClientError> {
        let token =
            self.config.webhook_token.as_ref().ok_or_else(|| N8nClientError::InvalidInput("n8n webhook token is not configured".into()))?;
        let resp =
            self.http.post(self.webhook_url(path)).header(N8N_WEBHOOK_TOKEN_HEADER, token.expose()).json(body).send().await.map_err(
                |e| {
                    if e.is_connect() || e.is_builder() {
                        N8nClientError::Transport(e.to_string())
                    } else {
                        N8nClientError::Ambiguous(e.to_string())
                    }
                },
            )?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| N8nClientError::Ambiguous(e.to_string()))?;
        if status == StatusCode::NOT_FOUND {
            return Err(N8nClientError::NotFound);
        }
        if !status.is_success() {
            return Err(N8nClientError::Api { status: status.as_u16(), message: text });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| N8nClientError::Parse(format!("n8n webhook body: {e}")))
    }
}

async fn decode_response<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, N8nClientError> {
    let status = resp.status();
    let body = resp.text().await.map_err(|e| N8nClientError::Transport(e.to_string()))?;
    if status == StatusCode::NOT_FOUND {
        return Err(N8nClientError::NotFound);
    }
    if !status.is_success() {
        return Err(N8nClientError::Api { status: status.as_u16(), message: body });
    }
    serde_json::from_str(&body).map_err(|e| N8nClientError::Parse(format!("n8n API body: {e}")))
}

pub type SharedN8nClient = Arc<N8nClient>;

// ── Fleetix Hub publishing contract v1 ─────────────────────────────────────
//
// Source of truth: `studio-pipeline/n8n/README.md` (contract v1, unverified
// against a live n8n). The only caller is this publisher.

/// Contract v1 webhook prefix; the platform is the next path segment:
/// `POST {base}/webhook/fleetix-hub/v1/<platform>`.
pub const N8N_PUBLISH_PREFIX: &str = "/webhook/fleetix-hub/v1";

/// Publishing configuration for contract v1.
///
/// One instance serves one platform: the lookup path carries the platform, but
/// [`Publisher::lookup`] only receives the account id and the idempotency key.
/// A channel per platform (`n8n:<platform>`) is therefore what a release
/// action's `transport` must name.
#[derive(Clone, PartialEq, Eq)]
pub struct N8nPublisherConfig {
    /// n8n base URL, no trailing slash.
    pub base_url: String,
    /// The token shared with the workflow's `N8N_WEBHOOK_TOKEN`; sent as
    /// `Authorization: Bearer …`.
    pub webhook_token: SecretString,
    /// Platform path segment (`youtube`, `bluesky`, `mastodon`, …).
    pub platform: String,
}

impl N8nPublisherConfig {
    pub fn new(base_url: impl Into<String>, webhook_token: impl Into<String>, platform: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            webhook_token: SecretString::new(webhook_token),
            platform: platform.into(),
        }
    }
}

impl std::fmt::Debug for N8nPublisherConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("N8nPublisherConfig")
            .field("base_url", &self.base_url)
            .field("platform", &self.platform)
            .field("webhook_token", &"<redacted>")
            .finish()
    }
}

/// n8n publishing transport. Submits a frozen release and reads its claim back;
/// `pending` is ambiguous, never success or failure.
pub struct N8nPublisher {
    http: Client,
    config: N8nPublisherConfig,
}

impl N8nPublisher {
    pub fn new(http: Client, config: N8nPublisherConfig) -> Self {
        Self { http, config }
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        rb.bearer_auth(self.config.webhook_token.expose())
    }

    fn webhook_url(&self, suffix: &str) -> String {
        format!("{}{}/{}{suffix}", self.config.base_url, N8N_PUBLISH_PREFIX, self.config.platform)
    }

    /// `POST …/v1/<platform>` with the JSON metadata part and one binary part
    /// per media item.
    fn publish_request(&self, req: &PublishRequest) -> PortResult<RequestBuilder> {
        if req.platform != self.config.platform {
            return Err(PortError::Rejected {
                status: 400,
                message: format!("n8n channel is configured for platform {:?}, not {:?}", self.config.platform, req.platform),
            });
        }
        let (content_type, body) = multipart_body(req)?;
        Ok(self.auth(self.http.post(self.webhook_url(""))).header(reqwest::header::CONTENT_TYPE, content_type).body(body))
    }

    /// `GET …/v1/<platform>/lookup?key=<idempotency_key>`.
    fn lookup_request(&self, key: &str) -> RequestBuilder {
        self.auth(self.http.get(self.webhook_url("/lookup"))).query(&[("key", key)])
    }

    /// Translate a reply into the contract-v1 publish shape. Mastodon is the
    /// only v1 platform that honours `reply_to` (as `in_reply_to_id`), so a
    /// reply goes through the same publish path and lookup as any other post.
    fn reply_request(&self, req: &ReplyRequest) -> PortResult<PublishRequest> {
        if self.config.platform != "mastodon" {
            return Err(PortError::Unsupported(format!(
                "n8n contract v1 answers a thread only for mastodon; this channel is {:?}",
                self.config.platform
            )));
        }
        let Some(reply_to) = req.in_reply_to.clone().filter(|id| !id.trim().is_empty()) else {
            return Err(PortError::Unsupported("a mastodon reply needs the status id it answers (in_reply_to)".into()));
        };
        Ok(PublishRequest {
            account_id: req.account_id.clone(),
            platform: self.config.platform.clone(),
            idempotency_key: req.idempotency_key.clone(),
            text: req.text.clone(),
            media: vec![],
            platform_settings: json!({}),
            reply_to: Some(reply_to),
        })
    }
}

#[async_trait]
impl Publisher for N8nPublisher {
    async fn submit(&self, req: &PublishRequest) -> PortResult<ProviderPost> {
        let resp = self.publish_request(req)?.send().await.map_err(|e| transport_error(e, true))?;
        let value = read_json(resp, false, true).await?;
        publish_result(&value, &req.idempotency_key)
    }

    async fn lookup(&self, _account_id: &str, idempotency_key: &str) -> PortResult<Option<ProviderPost>> {
        // A failed read after the request was sent leaves the claim state
        // unknown, so it is ambiguous rather than a retryable non-read.
        let resp = self.lookup_request(idempotency_key).send().await.map_err(|e| transport_error(e, true))?;
        let value = lookup_json(resp).await?;
        lookup_result(&value, idempotency_key)
    }

    async fn reply(&self, req: &ReplyRequest) -> PortResult<ProviderPost> {
        let publish = self.reply_request(req)?;
        let resp = self.publish_request(&publish)?.send().await.map_err(|e| transport_error(e, true))?;
        let value = read_json(resp, false, true).await?;
        publish_result(&value, &req.idempotency_key)
    }

    async fn lookup_reply(&self, account_id: &str, idempotency_key: &str) -> PortResult<Option<ProviderPost>> {
        if self.config.platform != "mastodon" {
            return Err(PortError::Unsupported(format!(
                "n8n contract v1 looks up a reply only for mastodon; this channel is {:?}",
                self.config.platform
            )));
        }
        self.lookup(account_id, idempotency_key).await
    }
}

/// Part name of media item `i`. The metadata's `media[i].part` and the
/// multipart part name must be identical, so both come from here.
fn media_part(index: usize) -> String {
    format!("media_{index}")
}

/// The `request` part: one JSON object describing the whole post.
fn request_part(req: &PublishRequest) -> Value {
    let media: Vec<Value> = req
        .media
        .iter()
        .enumerate()
        .map(|(i, blob)| {
            let mut entry = json!({ "part": media_part(i), "sha256": &blob.sha256, "mime": &blob.mime });
            if !blob.alt_text.trim().is_empty() {
                entry["alt_text"] = json!(&blob.alt_text);
            }
            entry
        })
        .collect();
    json!({
        "version": 1,
        "idempotency_key": &req.idempotency_key,
        "account_id": &req.account_id,
        "text": &req.text,
        "platform_settings": &req.platform_settings,
        "media": media,
        "reply_to": &req.reply_to,
    })
}

/// n8n stores an uploaded part under `binary[<part>]` only when the part has a
/// filename; no platform reads the extension.
fn media_filename(sha256: &str, mime: &str) -> String {
    let subtype = mime.split('/').nth(1).unwrap_or("bin").split('+').next().unwrap_or("bin");
    let ext = match subtype {
        "jpeg" => "jpg",
        "quicktime" => "mov",
        "mpeg" => "mp3",
        other if other.chars().all(|c| c.is_ascii_alphanumeric()) => other,
        _ => "bin",
    };
    format!("{sha256}.{ext}")
}

/// Byte-exact multipart body for contract v1. The workspace `reqwest` has no
/// `multipart` feature (the Postiz adapter hand-encodes for the same reason).
/// Returns the `Content-Type` value and the body.
fn multipart_body(req: &PublishRequest) -> PortResult<(String, Vec<u8>)> {
    // A fresh boundary cannot collide with arbitrary video bytes.
    let boundary = format!("fleetix-hub-{}", Uuid::new_v4().simple());
    let metadata = serde_json::to_vec(&request_part(req))
        .map_err(|e| PortError::Rejected { status: 400, message: format!("n8n request part is not serialisable: {e}") })?;
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(&metadata);
    body.extend_from_slice(b"\r\n");
    for (i, blob) in req.media.iter().enumerate() {
        let filename = media_filename(&blob.sha256, &blob.mime);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
                media_part(i),
                blob.mime
            )
            .as_bytes(),
        );
        body.extend_from_slice(&blob.bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok((format!("multipart/form-data; boundary={boundary}"), body))
}

/// Map a publish answer onto the port's result. `pending` is a claim with no
/// result: uncertain, so it stops the intent instead of reporting success or a
/// definite rejection.
fn publish_result(value: &Value, key: &str) -> PortResult<ProviderPost> {
    let state = value.get("state").and_then(Value::as_str).unwrap_or_default();
    let provider_id = value.get("provider_id").and_then(Value::as_str).unwrap_or_default();
    let url = value.get("url").and_then(Value::as_str).map(str::to_string);
    let raw = value.get("raw").cloned().unwrap_or(Value::Null);
    match state {
        "published" => {
            if provider_id.is_empty() {
                return Err(PortError::Ambiguous(format!("n8n published {key} without a provider_id")));
            }
            Ok(ProviderPost { provider_id: provider_id.to_string(), state: ProviderState::Published, url, raw })
        }
        "failed" => Ok(ProviderPost { provider_id: provider_id.to_string(), state: ProviderState::Failed, url, raw }),
        "pending" => Err(PortError::Ambiguous(format!("n8n claim for {key} is pending (no platform result); resolve it through lookup"))),
        "absent" => Err(PortError::Ambiguous(format!("n8n has no claim for {key} after a publish"))),
        other => Err(PortError::Ambiguous(format!("n8n publish answered state {other:?} for {key}"))),
    }
}

/// Read a contract-v1 lookup answer.
///
/// A missing lookup workflow (HTTP 404) is [`PortError::Unsupported`]: the
/// claim exists only in n8n and cannot be read, so the intent stops for an
/// operator. Any other non-2xx or malformed body is [`PortError::Ambiguous`]:
/// the claim state is unknown, never a definite failure.
async fn lookup_json(resp: reqwest::Response) -> PortResult<Value> {
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| PortError::Ambiguous(format!("reading n8n lookup body: {e}")))?;
    if status == 404 {
        return Err(PortError::Unsupported("n8n has no v1 lookup workflow (HTTP 404); a claimed post cannot be reconciled".into()));
    }
    if !(200..300).contains(&status) {
        return Err(PortError::Ambiguous(format!("n8n lookup answered HTTP {status}: {}", snippet(&bytes))));
    }
    serde_json::from_slice(&bytes).map_err(|e| PortError::Ambiguous(format!("n8n lookup body is not JSON: {e}")))
}

/// Map a lookup answer: `absent` is "no record", everything else reuses the
/// publish mapping so `pending` stays ambiguous.
fn lookup_result(value: &Value, key: &str) -> PortResult<Option<ProviderPost>> {
    if value.get("state").and_then(Value::as_str) == Some("absent") {
        return Ok(None);
    }
    publish_result(value, key).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_prefix_is_api_v1() {
        assert_eq!(N8N_API_PREFIX, "/api/v1");
    }

    #[test]
    fn url_header_name_is_n8n_specific() {
        assert_eq!(N8N_API_KEY_HEADER, "X-N8N-API-KEY");
    }

    #[test]
    fn client_trims_trailing_slash_from_base_url() {
        let client = N8nClient::new(reqwest::Client::new(), N8nConfig::new("https://n8n.example.com///", "ntn_x"));
        assert_eq!(client.config.n8n_base_url, "https://n8n.example.com");
    }

    #[test]
    fn url_for_concatenates_correctly() {
        let client = N8nClient::new(reqwest::Client::new(), N8nConfig::new("https://n8n.example.com", "k"));
        assert_eq!(client.url_for("/workflows"), "https://n8n.example.com/api/v1/workflows");
        assert_eq!(client.url_for("/workflows/abc/execute"), "https://n8n.example.com/api/v1/workflows/abc/execute");
    }

    #[test]
    fn webhook_url_skips_the_api_prefix_and_leading_slashes() {
        let client = N8nClient::new(reqwest::Client::new(), N8nConfig::webhook_only("http://127.0.0.1:5680/", "studio-token"));
        assert_eq!(client.webhook_url("/studio/publish"), "http://127.0.0.1:5680/webhook/studio/publish");
    }

    #[test]
    fn config_serde_roundtrip_keeps_secrets_usable() {
        let cfg = N8nConfig::new("https://n8n.example.com", "ntn_test").with_webhook_token("studio-token");
        let json = serde_json::to_value(&cfg).unwrap();
        let back: N8nConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.n8n_api_key.as_ref().unwrap().expose(), "ntn_test");
        assert_eq!(back.webhook_token.as_ref().unwrap().expose(), "studio-token");
    }

    #[test]
    fn debug_redacts_both_credentials() {
        let cfg = N8nConfig::new("https://n8n.example.com", "ntn_secret").with_webhook_token("studio-secret");
        let dbg = format!("{:?}", cfg);
        assert!(!dbg.contains("ntn_secret"), "api key leaked: {dbg}");
        assert!(!dbg.contains("studio-secret"), "webhook token leaked: {dbg}");
        assert_eq!(dbg.matches("<redacted>").count(), 2);
        assert_eq!(format!("{:?}", SecretString::new("raw-secret")), "<redacted>");
    }

    #[test]
    fn error_display_includes_status() {
        let e = N8nClientError::Api { status: 401, message: "Unauthorized".into() };
        assert!(e.to_string().contains("401"));
    }

    #[test]
    fn not_found_error_is_distinct_variant() {
        let e = N8nClientError::NotFound;
        assert!(e.to_string().contains("404"));
    }

    #[tokio::test]
    async fn webhook_without_a_token_fails_before_sending() {
        let client = N8nClient::new(reqwest::Client::new(), N8nConfig::new("http://127.0.0.1:5680", "ntn_key"));
        let err = client.trigger_webhook("studio/publish", &serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, N8nClientError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn rest_call_without_an_api_key_fails_before_sending() {
        let client = N8nClient::new(reqwest::Client::new(), N8nConfig::webhook_only("http://127.0.0.1:5680", "studio-token"));
        let err = client.get_json::<Value>("/workflows", &[]).await.unwrap_err();
        assert!(matches!(err, N8nClientError::InvalidInput(_)));
    }

    // ── contract v1 publisher ──────────────────────────────────────────────

    const CONTRACT_TOKEN: &str = "contract-secret-token";

    fn publisher() -> N8nPublisher {
        N8nPublisher::new(Client::new(), N8nPublisherConfig::new("http://127.0.0.1:5680/", CONTRACT_TOKEN, "bluesky"))
    }

    fn mastodon_publisher(base: &str) -> N8nPublisher {
        N8nPublisher::new(Client::new(), N8nPublisherConfig::new(base, CONTRACT_TOKEN, "mastodon"))
    }

    fn reply() -> ReplyRequest {
        ReplyRequest {
            account_id: "1098".into(),
            idempotency_key: "ihub-reply-draft-1".into(),
            thread_id: "status-42".into(),
            in_reply_to: Some("status-42".into()),
            text: "thanks for asking!".into(),
        }
    }

    fn request() -> PublishRequest {
        PublishRequest {
            account_id: "did:plc:studio".into(),
            platform: "bluesky".into(),
            idempotency_key: "release-attempt-1".into(),
            text: "verified post".into(),
            media: vec![
                domain::ports::MediaBlob {
                    sha256: "aa".repeat(32),
                    mime: "image/png".into(),
                    alt_text: "an alt".into(),
                    bytes: vec![0x89, b'P', b'N', b'G'],
                },
                domain::ports::MediaBlob {
                    sha256: "bb".repeat(32),
                    mime: "video/mp4".into(),
                    alt_text: String::new(),
                    bytes: b"\x00\x00\x00\x18ftypmp42".to_vec(),
                },
            ],
            platform_settings: json!({ "langs": ["en"], "paid_promotion": false }),
            reply_to: None,
        }
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// The JSON metadata part, located in the encoded body.
    fn request_json(body: &[u8], boundary: &str) -> Value {
        let head = b"name=\"request\"\r\nContent-Type: application/json\r\n\r\n";
        let start = find(body, head).expect("request part") + head.len();
        let end = find(&body[start..], format!("\r\n--{boundary}").as_bytes()).expect("end of request part") + start;
        serde_json::from_slice(&body[start..end]).expect("request part is JSON")
    }

    fn body_of(built: &reqwest::Request) -> &[u8] {
        built.body().and_then(reqwest::Body::as_bytes).expect("in-memory multipart body")
    }

    #[test]
    fn publish_request_uses_the_bearer_token_and_platform_path() {
        let built = publisher().publish_request(&request()).unwrap().build().unwrap();
        assert_eq!(built.method(), reqwest::Method::POST);
        assert_eq!(built.url().as_str(), "http://127.0.0.1:5680/webhook/fleetix-hub/v1/bluesky");
        assert_eq!(built.headers().get(reqwest::header::AUTHORIZATION).unwrap(), format!("Bearer {CONTRACT_TOKEN}").as_str());
        let content_type = built.headers().get(reqwest::header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(content_type.starts_with("multipart/form-data; boundary=fleetix-hub-"), "{content_type}");
    }

    #[test]
    fn multipart_body_carries_one_request_part_and_one_part_per_media_item() {
        let req = request();
        let built = publisher().publish_request(&req).unwrap().build().unwrap();
        let body = body_of(&built);
        let text = String::from_utf8_lossy(body);
        let content_type = built.headers().get(reqwest::header::CONTENT_TYPE).unwrap().to_str().unwrap();
        let boundary = content_type.split("boundary=").nth(1).expect("boundary parameter");
        assert_eq!(text.matches(&format!("--{boundary}\r\n")).count(), 3, "request + two media parts");
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
        assert!(text.contains("name=\"request\""));
        assert!(text.contains("name=\"media_0\"; filename=\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(text.contains("name=\"media_1\"; filename=\""));
        assert!(text.contains("Content-Type: video/mp4"));
        assert!(find(body, &req.media[0].bytes).is_some(), "media bytes are the approved payload, verbatim");
        assert!(find(body, &req.media[1].bytes).is_some());

        let meta = request_json(body, boundary);
        assert_eq!(meta["version"], 1);
        assert_eq!(meta["idempotency_key"], "release-attempt-1");
        assert_eq!(meta["account_id"], "did:plc:studio");
        assert_eq!(meta["text"], "verified post");
        assert_eq!(meta["platform_settings"], req.platform_settings);
        assert_eq!(meta["reply_to"], Value::Null);
        assert_eq!(meta["media"][0]["part"], "media_0");
        assert_eq!(meta["media"][0]["sha256"], req.media[0].sha256);
        assert_eq!(meta["media"][0]["mime"], "image/png");
        assert_eq!(meta["media"][0]["alt_text"], "an alt");
        assert_eq!(meta["media"][1]["part"], "media_1");
        assert_eq!(meta["media"][1]["mime"], "video/mp4");
        assert!(meta["media"][1].get("alt_text").is_none(), "blank alt text is omitted");
    }

    #[test]
    fn lookup_request_targets_the_platform_lookup_with_the_key() {
        let built = publisher().lookup_request("release-attempt-1").build().unwrap();
        assert_eq!(built.method(), reqwest::Method::GET);
        assert_eq!(built.url().path(), "/webhook/fleetix-hub/v1/bluesky/lookup");
        assert_eq!(built.url().query(), Some("key=release-attempt-1"));
        assert_eq!(built.headers().get(reqwest::header::AUTHORIZATION).unwrap().to_str().unwrap(), format!("Bearer {CONTRACT_TOKEN}"));
    }

    #[test]
    fn a_request_for_another_platform_never_reaches_the_wire() {
        let mut req = request();
        req.platform = "youtube".into();
        assert!(matches!(publisher().publish_request(&req), Err(PortError::Rejected { status: 400, .. })));
    }

    #[test]
    fn a_mastodon_reply_becomes_a_contract_v1_publish_that_answers_the_status() {
        let mastodon = mastodon_publisher("http://127.0.0.1:5680");
        let publish = mastodon.reply_request(&reply()).unwrap();
        let built = mastodon.publish_request(&publish).unwrap().build().unwrap();
        assert_eq!(built.method(), reqwest::Method::POST);
        assert_eq!(built.url().as_str(), "http://127.0.0.1:5680/webhook/fleetix-hub/v1/mastodon");

        let body = body_of(&built);
        let content_type = built.headers().get(reqwest::header::CONTENT_TYPE).unwrap().to_str().unwrap();
        let boundary = content_type.split("boundary=").nth(1).expect("boundary parameter");
        let meta = request_json(body, boundary);
        assert_eq!(meta["idempotency_key"], "ihub-reply-draft-1", "the reply idempotency key is the claim key");
        assert_eq!(meta["account_id"], "1098");
        assert_eq!(meta["text"], "thanks for asking!");
        assert_eq!(meta["reply_to"], "status-42", "Mastodon answers in_reply_to_id with the parent status id");
        assert_eq!(meta["media"], json!([]), "a reply freezes no new media");
        assert_eq!(meta["platform_settings"], json!({}), "the channel's documented defaults apply");
    }

    #[test]
    fn only_mastodon_answers_a_thread_and_a_parent_is_required() {
        let err = publisher().reply_request(&reply()).unwrap_err();
        assert!(matches!(&err, PortError::Unsupported(m) if m.contains("mastodon")), "{err}");

        let mastodon = mastodon_publisher("http://127.0.0.1:5680");
        let err = mastodon.reply_request(&ReplyRequest { in_reply_to: None, ..reply() }).unwrap_err();
        assert!(matches!(&err, PortError::Unsupported(m) if m.contains("in_reply_to")), "{err}");
        let err = mastodon.reply_request(&ReplyRequest { in_reply_to: Some("  ".into()), ..reply() }).unwrap_err();
        assert!(matches!(err, PortError::Unsupported(_)), "a blank parent id is not a reply");
    }

    #[tokio::test]
    async fn a_reply_maps_its_answer_through_the_publish_result() {
        let base =
            serve_once("200 OK", r#"{"state":"published","provider_id":"status-99","url":"https://social/@me/99","raw":null}"#).await;
        let post = mastodon_publisher(&base).reply(&reply()).await.unwrap();
        assert_eq!(post.provider_id, "status-99");
        assert_eq!(post.state, ProviderState::Published);
        assert_eq!(post.url.as_deref(), Some("https://social/@me/99"));

        let base = serve_once("200 OK", r#"{"state":"pending","provider_id":null,"url":null,"raw":null}"#).await;
        let pending = mastodon_publisher(&base).reply(&reply()).await.unwrap_err();
        assert!(matches!(pending, PortError::Ambiguous(_)), "a pending claim is uncertain, never a reply: {pending}");
    }

    #[tokio::test]
    async fn a_reply_lookup_reads_the_platform_lookup_and_stays_unsupported_elsewhere() {
        let base = serve_once("200 OK", r#"{"state":"published","provider_id":"status-99","url":null,"raw":null}"#).await;
        let found = mastodon_publisher(&base).lookup_reply("1098", "ihub-reply-draft-1").await.unwrap().unwrap();
        assert_eq!(found.provider_id, "status-99");

        let err = publisher().lookup_reply("did:plc:studio", "ihub-reply-draft-1").await.unwrap_err();
        assert!(matches!(&err, PortError::Unsupported(m) if m.contains("mastodon")), "{err}");
    }

    #[test]
    fn publish_maps_published_failed_and_pending() {
        let published = publish_result(
            &json!({"state": "published", "provider_id": "at://x", "url": "https://bsky.app/x", "raw": {"uri": "at://x"}}),
            "k",
        )
        .unwrap();
        assert_eq!(published.provider_id, "at://x");
        assert_eq!(published.state, ProviderState::Published);
        assert_eq!(published.url.as_deref(), Some("https://bsky.app/x"));
        assert_eq!(published.raw, json!({"uri": "at://x"}));

        let failed = publish_result(&json!({"state": "failed", "provider_id": null, "url": null, "raw": {"error": "nope"}}), "k").unwrap();
        assert_eq!(failed.state, ProviderState::Failed);
        assert_eq!(failed.provider_id, "", "a rejected post has no provider id");
        assert_eq!(failed.raw, json!({"error": "nope"}));

        // A claim with no result is uncertain: neither success nor failure.
        assert!(matches!(publish_result(&json!({"state": "pending"}), "k"), Err(PortError::Ambiguous(_))));
        assert!(matches!(publish_result(&json!({"state": "absent"}), "k"), Err(PortError::Ambiguous(_))));
        assert!(matches!(publish_result(&json!({"state": "surprise"}), "k"), Err(PortError::Ambiguous(_))));
        assert!(matches!(publish_result(&json!({}), "k"), Err(PortError::Ambiguous(_))));
        assert!(
            matches!(publish_result(&json!({"state": "published", "provider_id": null}), "k"), Err(PortError::Ambiguous(_))),
            "published without an id is a malformed success"
        );
    }

    #[test]
    fn lookup_maps_absent_to_no_record_and_pending_to_ambiguous() {
        assert!(lookup_result(&json!({"state": "absent", "provider_id": null, "url": null, "raw": null}), "k").unwrap().is_none());

        let published = lookup_result(&json!({"state": "published", "provider_id": "p1", "url": "u", "raw": null}), "k").unwrap().unwrap();
        assert_eq!(published.state, ProviderState::Published);
        assert_eq!(published.provider_id, "p1");

        let failed = lookup_result(&json!({"state": "failed", "provider_id": null, "url": null, "raw": {"e": 1}}), "k").unwrap().unwrap();
        assert_eq!(failed.state, ProviderState::Failed);

        assert!(
            matches!(
                lookup_result(&json!({"state": "pending", "provider_id": null, "url": null, "raw": null}), "k"),
                Err(PortError::Ambiguous(_))
            ),
            "pending lookup is ambiguous, not a success and not a failure"
        );
    }

    /// Serve exactly one raw HTTP answer and run a lookup against it.
    async fn lookup_served_by(status: &'static str, body: &'static str) -> PortResult<Option<ProviderPost>> {
        let base = serve_once(status, body).await;
        let publisher = N8nPublisher::new(Client::new(), N8nPublisherConfig::new(base, CONTRACT_TOKEN, "bluesky"));
        publisher.lookup("did:plc:studio", "release-attempt-1").await
    }

    /// Serve exactly one raw HTTP answer and return the base URL.
    async fn serve_once(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_missing_lookup_workflow_is_unsupported_not_absent() {
        let err = lookup_served_by("404 Not Found", "").await.unwrap_err();
        assert!(
            matches!(err, PortError::Unsupported(_)),
            "a crashed claim must stay resolvable by the operator, never read as 'no record': {err}"
        );
    }

    #[tokio::test]
    async fn uncertain_lookup_answers_are_ambiguous() {
        for (status, body) in [("502 Bad Gateway", "bad gateway"), ("401 Unauthorized", r#"{"error":"nope"}"#), ("200 OK", "{ not json")] {
            let err = lookup_served_by(status, body).await.unwrap_err();
            assert!(matches!(err, PortError::Ambiguous(_)), "{status} {body} must be ambiguous, got {err}");
        }
    }

    #[tokio::test]
    async fn lookup_reads_the_claim_states() {
        let published =
            lookup_served_by("200 OK", r#"{"state":"published","provider_id":"p1","url":"u","raw":null}"#).await.unwrap().unwrap();
        assert_eq!(published.state, ProviderState::Published);
        assert_eq!(published.provider_id, "p1");

        let absent = lookup_served_by("200 OK", r#"{"state":"absent","provider_id":null,"url":null,"raw":null}"#).await.unwrap();
        assert!(absent.is_none(), "only an explicit absent means the provider has no record");

        let pending = lookup_served_by("200 OK", r#"{"state":"pending","provider_id":null,"url":null,"raw":null}"#).await.unwrap_err();
        assert!(matches!(pending, PortError::Ambiguous(_)), "pending is uncertain: {pending}");
    }

    #[test]
    fn config_debug_redacts_the_contract_token() {
        let dbg = format!("{:?}", publisher().config);
        assert!(!dbg.contains(CONTRACT_TOKEN), "token leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains("bluesky"));
    }
}
