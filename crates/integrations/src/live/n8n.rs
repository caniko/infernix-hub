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

use std::sync::Arc;

use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

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
}
