//! Live adapters (owned by the integrations track).
//!
//! Every adapter funnels transport failures through [`transport_error`] so the
//! hub can tell "never reached the service" from "may have been applied":
//!
//! * connect refused / DNS / connect timeout before anything was sent →
//!   [`PortError::Unreachable`];
//! * timeout or reset after the request (body) was sent, or a response body
//!   read failure → [`PortError::Ambiguous`];
//! * HTTP 4xx, and 5xx on a read → [`PortError::Rejected`]; 5xx on a write →
//!   [`PortError::Ambiguous`];
//! * 404 on a read → [`PortError::NotFound`].
//!
//! Secrets never appear in `Debug`: every config holding a token has a manual
//! `Debug` that prints `<redacted>`, and no adapter logs headers or bodies that
//! could contain one.

use domain::ports::PortError;
use serde_json::Value;

pub mod comfy;
pub mod n8n;
pub mod paperclip;
pub mod postiz;
pub mod windmill;

pub use comfy::ComfyClient;
pub use n8n::{N8nClient, N8nClientError, N8nConfig, SecretString};
pub use paperclip::{PaperclipClient, PaperclipConfig};
pub use postiz::{PostizClient, PostizConfig};
pub use windmill::{WindmillClient, WindmillConfig};

/// Largest body snippet echoed into an error. Error strings reach logs, so keep
/// them small and never include request headers.
const BODY_SNIPPET: usize = 500;

fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out: String = text.chars().take(BODY_SNIPPET).collect();
    if text.chars().count() > BODY_SNIPPET {
        out.push('…');
    }
    out
}

/// Classify a `reqwest` send error.
///
/// `body_sent` is true for requests whose body may already have left the
/// process (POST/PUT/PATCH). Connect-stage failures are always safe to retry;
/// everything else on a write is ambiguous and must be reconciled first.
pub(crate) fn transport_error(err: reqwest::Error, body_sent: bool) -> PortError {
    if err.is_connect() || err.is_builder() {
        return PortError::Unreachable(err.to_string());
    }
    if body_sent { PortError::Ambiguous(err.to_string()) } else { PortError::Unreachable(err.to_string()) }
}

/// A 5xx on a write does not prove the server did nothing (a gateway can fail
/// after the upstream applied it), so it is ambiguous, never a rejection.
fn status_error(status: u16, bytes: &[u8], write: bool) -> PortError {
    if write && status >= 500 {
        return PortError::Ambiguous(format!("HTTP {status} on a write: {}", snippet(bytes)));
    }
    PortError::Rejected { status, message: snippet(bytes) }
}

/// Read a response as JSON.
///
/// * `not_found` maps 404 to [`PortError::NotFound`] (reads only).
/// * `write` marks the call as a mutation: a body read or parse failure then
///   becomes [`PortError::Ambiguous`], because the server may have applied it.
pub(crate) async fn read_json(resp: reqwest::Response, not_found: bool, write: bool) -> Result<Value, PortError> {
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| PortError::Ambiguous(format!("reading response body: {e}")))?;
    if not_found && status == 404 {
        return Err(PortError::NotFound);
    }
    if !(200..300).contains(&status) {
        return Err(status_error(status, &bytes, write));
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        let message = format!("invalid JSON response: {e}; body: {}", snippet(&bytes));
        if write { PortError::Ambiguous(message) } else { PortError::Rejected { status, message } }
    })
}

/// Read a response as text (Windmill returns bare uuids and plain-text bodies).
pub(crate) async fn read_text(resp: reqwest::Response, not_found: bool, write: bool) -> Result<String, PortError> {
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| PortError::Ambiguous(format!("reading response body: {e}")))?;
    if not_found && status == 404 {
        return Err(PortError::NotFound);
    }
    if !(200..300).contains(&status) {
        return Err(status_error(status, &bytes, write));
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if text.is_empty() && write {
        // A 2xx with no body on a mutation is still a success receipt: the
        // server answered. Callers that need an id validate it themselves.
        return Ok(text);
    }
    Ok(text)
}

/// Read raw bytes (ComfyUI `/view`).
pub(crate) async fn read_bytes(resp: reqwest::Response, not_found: bool) -> Result<Vec<u8>, PortError> {
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| PortError::Ambiguous(format!("reading response body: {e}")))?;
    if not_found && status == 404 {
        return Err(PortError::NotFound);
    }
    if !(200..300).contains(&status) {
        return Err(PortError::Rejected { status, message: snippet(&bytes) });
    }
    Ok(bytes.to_vec())
}

/// Trim a base URL without a trailing slash so path joins stay single-slash.
pub(crate) fn trim_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_errors_on_writes_are_ambiguous() {
        assert!(matches!(status_error(502, b"bad gateway", true), PortError::Ambiguous(_)));
        assert!(matches!(status_error(502, b"bad gateway", false), PortError::Rejected { status: 502, .. }));
        assert!(matches!(status_error(409, b"conflict", true), PortError::Rejected { status: 409, .. }));
    }

    #[test]
    fn trims_every_trailing_slash() {
        assert_eq!(trim_base("http://127.0.0.1:3117///"), "http://127.0.0.1:3117");
        assert_eq!(trim_base("http://127.0.0.1:3117"), "http://127.0.0.1:3117");
    }

    #[test]
    fn snippet_is_bounded() {
        let long = "x".repeat(BODY_SNIPPET + 10);
        assert_eq!(snippet(long.as_bytes()).chars().count(), BODY_SNIPPET + 1);
    }
}
