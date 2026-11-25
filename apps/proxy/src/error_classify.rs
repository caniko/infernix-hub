use std::time::Duration;

use axum::http::{HeaderMap, StatusCode, header};
use chrono::Utc;
pub(crate) use contracts::providers::sanitize_provider_error as sanitize;
use contracts::providers::{AccountError, AccountErrorKind, AccountErrorSource};
use serde_json::Value;

use crate::pool::{Account, Lane, text};

use AccountErrorKind::*;
use AccountErrorSource::*;

pub(crate) const ERROR_BODY_LIMIT: usize = 64 * 1024;

pub(crate) fn blocking_backoff(kind: AccountErrorKind) -> Option<Duration> {
    match kind {
        VerificationRequired | AccountSuspended | AuthExpired | BillingRequired => Some(Duration::from_secs(900)),
        PermissionDenied => Some(Duration::from_secs(600)),
        _ => None,
    }
}

pub(crate) fn local_error(kind: AccountErrorKind, source: AccountErrorSource, message: &str) -> AccountError {
    AccountError {
        kind,
        source,
        http_status: None,
        provider_code: None,
        message: message.into(),
        action_hint: None,
        action_url: None,
        occurred_at: Utc::now(),
        consecutive_count: 0,
        backoff_until: None,
    }
}

pub(crate) fn probe_error(message: &str) -> Box<AccountError> {
    Box::new(local_error(ProbeFailed, QuotaProbe, message))
}

pub(crate) fn safe_label(account: &Account) -> String {
    scrub_label(&account.label, &account_secrets(account))
}

pub(crate) fn scrub_label(label: &str, secrets: &[&str]) -> String {
    sanitize(&redact_known_secrets(label, secrets))
}

pub(crate) fn account_secrets(account: &Account) -> [&str; 3] {
    account.current_secrets()
}

fn redact_known_secrets(mut text: &str, secrets: &[&str]) -> String {
    let mut out = String::new();
    while let Some((start, len)) = secrets
        .iter()
        .filter(|s| !s.is_empty())
        .filter_map(|s| Some((text.find(s)?, s.len())))
        .min_by_key(|&(start, len)| (start, std::cmp::Reverse(len)))
    {
        out.push_str(&text[..start]);
        out.push_str("[redacted]");
        text = &text[start + len..];
    }
    out.push_str(text);
    out
}

pub(crate) fn classify_upstream_http_error(
    lane: Lane,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    source: AccountErrorSource,
    secrets: &[&str],
) -> Option<AccountError> {
    if status.is_success() {
        return None;
    }
    let raw: Value = serde_json::from_slice(body).unwrap_or_default();
    let body = String::from_utf8_lossy(body);
    let html = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/html"))
        || body.trim_start().starts_with('<');
    classify(lane, Some(status.as_u16()), &raw, &body, html, source, secrets)
}

pub(crate) fn classify_oauth_error(lane: Lane, status: StatusCode, body: &[u8], secrets: &[&str]) -> AccountError {
    classify_upstream_http_error(lane, status, &HeaderMap::new(), body, OAuthRefresh, secrets)
        .unwrap_or_else(|| local_error(Other, OAuthRefresh, "OAuth refresh was rejected."))
}

pub(crate) fn classify_sse_error(lane: Lane, event_name: &str, data: &[u8], secrets: &[&str]) -> Option<AccountError> {
    let raw: Value = serde_json::from_slice(data).unwrap_or_default();
    if event_name != "error"
        && event_name != "response.failed"
        && !matches!(raw["type"].as_str(), Some("error" | "response.failed"))
        && raw["error"].is_null()
        && raw.pointer("/response/error").is_none_or(Value::is_null)
    {
        return None;
    }
    let body = String::from_utf8_lossy(data);
    classify(lane, None, &raw, &body, body.trim_start().starts_with('<'), Stream, secrets)
}

fn error_payload(raw: &Value) -> &Value {
    raw.get("error").or_else(|| raw.pointer("/response/error")).unwrap_or(raw)
}

fn numeric_code(error: &Value) -> Option<u16> {
    error.get("code").and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok())
}

// The outer Option records whether the array contained a structured error at all.
fn google_array_error(
    items: &[Value],
    status: Option<u16>,
    html: bool,
    source: AccountErrorSource,
    secrets: &[&str],
) -> Option<Option<AccountError>> {
    items
        .iter()
        .filter_map(|item| {
            if let Some(items) = item.as_array() {
                return google_array_error(items, status, html, source, secrets);
            }
            let wrapped = item.get("error").or_else(|| item.pointer("/response/error"));
            let code = numeric_code(wrapped.unwrap_or(item));
            (wrapped.is_some_and(Value::is_object)
                || (wrapped.is_none()
                    && (text(item, &["status"]).is_some_and(|s| !s.trim().eq_ignore_ascii_case("OK"))
                        || ["code", "details", "message"].iter().any(|key| item.get(key).is_some()))))
            .then(|| classify(Lane::Google, code.or(status), item, "", html, source, secrets))
        })
        // A quota entry must not hide an account-level rejection in the same response.
        .min_by_key(|error| match error {
            Some(error) if error.kind.is_blocking() => 0,
            Some(_) => 1,
            None => 2,
        })
}

fn classify(
    lane: Lane,
    status: Option<u16>,
    raw: &Value,
    body: &str,
    html: bool,
    source: AccountErrorSource,
    secrets: &[&str],
) -> Option<AccountError> {
    if lane == Lane::Google
        && let Some(items) = raw.as_array()
    {
        return google_array_error(items, status, html, source, secrets)
            .unwrap_or_else(|| status.and_then(|_| classify(lane, status, &Value::Null, "", html, source, secrets)));
    }
    let error = error_payload(raw);
    let rpc_status = (lane == Lane::Google).then(|| text(error, &["status"])).flatten();
    let status = status.or_else(|| numeric_code(error)).and_then(|n| StatusCode::from_u16(n).ok());
    let http = status.map(|s| s.as_u16());
    let mut reasons = Vec::new();
    let mut validation = false;
    let mut help = None;
    if lane == Lane::Google {
        for detail in error["details"].as_array().into_iter().flatten() {
            match text(detail, &["@type"]).and_then(|s| s.rsplit('/').next()) {
                Some("google.rpc.ErrorInfo" | "ErrorInfo") => {
                    reasons.extend(text(detail, &["reason"]));
                    validation |= text(detail, &["/metadata/validation_url", "/metadata/validationUrl"]).is_some();
                }
                Some("google.rpc.Help" | "Help") => {
                    help = detail["links"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|link| text(link, &["url"]))
                        .find_map(|url| help_url(&redact_known_secrets(url, secrets)));
                }
                _ => {}
            }
        }
    }
    let code = text(error, &["code"]).or_else(|| text(raw, &["error"]));
    let error_type = text(error, &["type"]).filter(|s| *s != "error");
    let message = text(error, &["message", "error_description", "detail"])
        .or_else(|| text(raw, &["error_description", "message", "detail"]))
        .or_else(|| error.as_str())
        .unwrap_or(if html || !raw.is_null() { "" } else { body });
    let message = redact_known_secrets(message, secrets);
    let lower = message.to_ascii_lowercase();
    if lane == Lane::Claude
        && source == QuotaProbe
        && http == Some(403)
        && lower.trim().trim_end_matches('.') == "oauth authentication is currently not allowed for this organization"
    {
        let mut result = local_error(QuotaProbeUnsupported, source, "Organization policy disallows OAuth quota probes.");
        result.http_status = http;
        result.backoff_until = Some(result.occurred_at + chrono::TimeDelta::hours(1));
        return Some(result);
    }
    let says = |phrases: &[&str]| phrases.iter().any(|p| lower.contains(p));
    let codes = || reasons.iter().copied().chain(code).chain(error_type).chain(rpc_status);
    let has = |known: &[&str]| codes().any(|code| known.iter().any(|s| code.eq_ignore_ascii_case(s)));
    // A 5xx response from a refresh endpoint is not evidence the login was revoked.
    let client = http.is_none_or(|n| (400..500).contains(&n));
    let restricted = http.is_none_or(|n| matches!(n, 401 | 402 | 403 | 429));
    let verification = validation
        || has(&[
            "validation_required",
            "verification_required",
            "account_verification_required",
            "captcha_required",
            "challenge_required",
            "captcha",
            "challenge",
        ])
        || (restricted
            && says(&[
                "verify your account",
                "account verification",
                "verification required",
                "captcha",
                "challenge required",
                "verification challenge",
            ]));
    let kind = if source == OAuthRefresh && status.is_some_and(|s| s.is_server_error() || s == StatusCode::REQUEST_TIMEOUT) {
        UpstreamUnavailable
    } else if source == OAuthRefresh {
        if has(&["invalid_grant", "invalid_client", "unauthorized_client"]) { AuthExpired } else { Other }
    } else if http == Some(400) {
        // Anthropic wraps this specific account error in invalid_request_error.
        // Match the canonical message, not a phrase quoted in input validation.
        if lane == Lane::Claude && (lower.starts_with("your credit balance is too low") || lower.starts_with("credit balance is too low")) {
            BillingRequired
        } else if has(&["invalid_request_error", "INVALID_ARGUMENT", "bad_request", "validation_error"]) {
            return None;
        } else if lane == Lane::Google
            && has(&["FAILED_PRECONDITION"])
            && says(&["user location is not supported", "not available in your country"])
        {
            PermissionDenied
        } else {
            return None;
        }
    } else if source == Inference && matches!(http, Some(404 | 422)) {
        return None;
    } else if verification {
        VerificationRequired
    } else if has(&["account_suspended", "account_deactivated", "consumer_suspended", "organization_suspended", "tos_violation"])
        || (restricted
            && says(&[
                "account suspended",
                "account is suspended",
                "account has been suspended",
                "account has been banned",
                "disabled for violating",
            ]))
        || (http.is_none_or(|n| n == 403) && says(&["suspended", "banned", "deactivated", "violation of", "policy violation"]))
    {
        AccountSuspended
    } else if http == Some(402)
        || has(&["billing_disabled", "billing_error", "insufficient_quota", "billing_hard_limit_reached", "billing_not_active"])
        || (restricted
            && says(&[
                "credit balance is too low",
                "credit balance too low",
                "insufficient balance",
                "insufficient credit",
                "purchase credits",
                "payment required",
                "exceeded your current quota",
                "check your plan and billing",
                "billing is disabled",
                "enable billing",
            ]))
    {
        BillingRequired
    } else if http == Some(401)
        || has(&[
            "unauthenticated",
            "authentication_error",
            "invalid_grant",
            "invalid_client",
            "unauthorized_client",
            "invalid_api_key",
            "invalid_token",
            "token_expired",
            "session_expired",
            "token_revoked",
        ])
    {
        AuthExpired
    } else if http == Some(429)
        || has(&["resource_exhausted", "rate_limit_exceeded", "quota_exceeded", "rate_limit_error", "usage_limit_reached"])
    {
        QuotaExhausted
    } else if http == Some(403)
        || has(&[
            "permission_denied",
            "permission_error",
            "service_disabled",
            "user_project_denied",
            "location_unsupported",
            "unsupported_country_region_territory",
            "insufficient_permissions",
        ])
        || (client
            && says(&[
                "location is not supported",
                "location not supported",
                "location is unsupported",
                "unsupported location",
                "unsupported country",
                "unsupported region",
            ]))
    {
        PermissionDenied
    } else if http == Some(408)
        || status.is_some_and(|s| s.is_server_error())
        || has(&["internal", "unavailable", "deadline_exceeded", "overloaded_error", "api_error", "server_error"])
    {
        UpstreamUnavailable
    } else {
        match source {
            QuotaProbe => ProbeFailed,
            Inference if client => return None,
            _ => Other,
        }
    };
    let mut result = local_error(kind, source, "");
    result.http_status = http;
    result.provider_code = if kind == VerificationRequired && lane == Lane::Google {
        Some("VALIDATION_REQUIRED".into())
    } else {
        codes()
            .find(|s| {
                s.len() <= 64
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    && redact_known_secrets(s, secrets) == *s
                    && sanitize(s) == *s
            })
            .map(str::to_owned)
    };
    let message = sanitize(&message);
    result.message = if html {
        if body.to_ascii_lowercase().contains("cloudflare") {
            "Upstream returned a Cloudflare error page."
        } else {
            "Upstream returned an HTML error page."
        }
        .into()
    } else if message.is_empty() {
        status.and_then(|s| s.canonical_reason()).unwrap_or("Upstream reported an error.").into()
    } else {
        message
    };
    if kind == VerificationRequired && lane == Lane::Google {
        result.action_hint =
            Some("Sign in to Antigravity (antigravity.google) as this Google account and complete account verification.".into());
        result.action_url = Some("https://antigravity.google".into());
    } else {
        result.action_url = help;
    }
    Some(result)
}

fn help_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    Some(
        match url.host_str()? {
            "antigravity.google" => "https://antigravity.google",
            "support.google.com" => "https://support.google.com",
            "console.cloud.google.com" => "https://console.cloud.google.com",
            _ => return None,
        }
        .into(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn structured_client_validation_does_not_block() {
        for word in ["captcha", "verify your account", "billing", "invalid value: 'credit balance is too low'"] {
            let body = serde_json::json!({"error":{"type":"invalid_request_error","message":word}});
            assert!(
                classify_upstream_http_error(
                    Lane::Claude,
                    StatusCode::BAD_REQUEST,
                    &HeaderMap::new(),
                    body.to_string().as_bytes(),
                    Inference,
                    &[],
                )
                .is_none()
            );
        }
    }

    #[test]
    fn opaque_account_secrets_are_redacted_before_truncation() {
        let secret = "opaque-credential-used-only-in-this-test";
        let body = format!("{} {secret}", "a".repeat(230));
        let error =
            classify_upstream_http_error(Lane::Claude, StatusCode::FORBIDDEN, &HeaderMap::new(), body.as_bytes(), Inference, &[secret])
                .unwrap();
        assert!(error.message.ends_with("[redacted"));
        assert!(!error.message.contains("opaque"));
    }

    pub(crate) const GOOGLE_VERIFICATION: &str = r#"{
      "error": {
        "code": 403, "message": "Verify your account to continue.", "status": "PERMISSION_DENIED",
        "details": [
          {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"VALIDATION_REQUIRED",
           "domain":"cloudcode-pa.googleapis.com",
           "metadata":{"validation_url":"https://accounts.google.com/signin/continue?token=private-query","validation_url_link_text":"Verify your account"}},
          {"@type":"type.googleapis.com/google.rpc.Help","links":[{"description":"Verify your account","url":"https://accounts.google.com/signin/continue?token=private-query"}]}
        ]
      }
    }"#;

    fn http(lane: Lane, status: u16, body: &str, source: AccountErrorSource) -> Option<AccountError> {
        classify_upstream_http_error(lane, StatusCode::from_u16(status).unwrap(), &HeaderMap::new(), body.as_bytes(), source, &[])
    }

    #[test]
    fn google_rpc_verification_and_other_reasons() {
        let error = http(Lane::Google, 403, GOOGLE_VERIFICATION, Inference).unwrap();
        assert_eq!(error.kind, VerificationRequired);
        assert_eq!(error.http_status, Some(403));
        assert_eq!(error.provider_code.as_deref(), Some("VALIDATION_REQUIRED"));
        assert_eq!(error.action_url.as_deref(), Some("https://antigravity.google"));
        assert_eq!(error.summary(), "Verification required: Verify your account to continue.");
        assert!(!serde_json::to_string(&error).unwrap().contains("private-query"));
        for (status, body, kind) in [
            (
                403,
                r#"{"error":{"details":[{"@type":"google.rpc.ErrorInfo","metadata":{"validation_url":"https://accounts.google.com/?secret=1"}}]}}"#,
                VerificationRequired,
            ),
            (403, r#"{"error":{"details":[{"@type":"google.rpc.ErrorInfo","reason":"CONSUMER_SUSPENDED"}]}}"#, AccountSuspended),
            (403, r#"{"error":{"details":[{"@type":"google.rpc.ErrorInfo","reason":"BILLING_DISABLED"}]}}"#, BillingRequired),
            (401, r#"{"error":{"status":"UNAUTHENTICATED"}}"#, AuthExpired),
            (429, r#"{"error":{"status":"RESOURCE_EXHAUSTED"}}"#, QuotaExhausted),
            (400, r#"{"error":{"status":"FAILED_PRECONDITION","message":"User location is not supported."}}"#, PermissionDenied),
            (503, r#"{"error":{"status":"UNAVAILABLE"}}"#, UpstreamUnavailable),
        ] {
            assert_eq!(http(Lane::Google, status, body, Inference).unwrap().kind, kind, "{body}");
        }
        assert_eq!(http(Lane::Google, 404, "", QuotaProbe).unwrap().kind, ProbeFailed);
        assert_eq!(http(Lane::Google, 302, "", Inference).unwrap().kind, Other);
    }

    #[test]
    fn google_empty_error_array_falls_back_to_http_status() {
        for body in ["[]", "[null, [], 42]"] {
            let error = http(Lane::Google, 403, body, Inference).unwrap();
            assert_eq!((error.kind, error.http_status), (PermissionDenied, Some(403)));
        }
    }

    #[test]
    fn google_array_uses_structured_errors_before_forbidden_http_status() {
        for (body, expected) in [
            ("[]", Some(PermissionDenied)),
            (r#"[{"status":"RESOURCE_EXHAUSTED"}]"#, Some(QuotaExhausted)),
            (r#"[{"code":"RESOURCE_EXHAUSTED"}]"#, Some(QuotaExhausted)),
            (r#"[{"details":[{"@type":"google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"}]}]"#, Some(QuotaExhausted)),
            (r#"[{"message":"Verify your account to continue."}]"#, Some(VerificationRequired)),
            (r#"[{"error":{"code":400,"status":"INVALID_ARGUMENT"}}]"#, None),
            (r#"[{},[{"error":{"code":400,"status":"INVALID_ARGUMENT"}}]]"#, None),
            (r#"[{"status":"OK"},{"error":{"code":400,"status":"INVALID_ARGUMENT"}}]"#, None),
            (r#"[{"status":" "},{"status":null},{"error":{"code":400,"status":"INVALID_ARGUMENT"}}]"#, None),
        ] {
            assert_eq!(http(Lane::Google, 403, body, Inference).map(|error| error.kind), expected, "{body}");
        }
    }

    #[test]
    fn anthropic_openai_and_go_distinguish_billing_and_client_errors() {
        for (lane, status, body, expected) in [
            (
                Lane::Claude,
                400,
                r#"{"error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#,
                Some(BillingRequired),
            ),
            (Lane::Claude, 401, r#"{"error":{"type":"authentication_error"}}"#, Some(AuthExpired)),
            (Lane::Claude, 403, r#"{"error":{"type":"permission_error"}}"#, Some(PermissionDenied)),
            (Lane::Claude, 403, "This account has been banned", Some(AccountSuspended)),
            (Lane::Claude, 429, r#"{"error":{"type":"rate_limit_error"}}"#, Some(QuotaExhausted)),
            (Lane::Claude, 529, r#"{"error":{"type":"overloaded_error"}}"#, Some(UpstreamUnavailable)),
            (Lane::Claude, 400, r#"{"error":{"type":"invalid_request_error","message":"context too long"}}"#, None),
            (Lane::Claude, 404, r#"{"error":{"type":"not_found_error"}}"#, None),
            (Lane::Codex, 429, r#"{"error":{"code":"insufficient_quota"}}"#, Some(BillingRequired)),
            (Lane::Codex, 403, r#"{"error":{"code":"account_deactivated"}}"#, Some(AccountSuspended)),
            (Lane::Codex, 403, r#"{"error":{"code":"unsupported_country_region_territory"}}"#, Some(PermissionDenied)),
            (Lane::Codex, 401, r#"{"error":{"code":"invalid_api_key"}}"#, Some(AuthExpired)),
            (Lane::Codex, 429, r#"{"error":{"type":"usage_limit_reached"}}"#, Some(QuotaExhausted)),
            (Lane::Codex, 400, r#"{"error":{"code":"invalid_request"}}"#, None),
            (Lane::Codex, 422, "invalid input", None),
            (Lane::Go, 401, "invalid key", Some(AuthExpired)),
            (Lane::Go, 403, "CAPTCHA required", Some(VerificationRequired)),
            (Lane::Go, 403, "insufficient balance", Some(BillingRequired)),
            (Lane::Go, 429, "rate limit", Some(QuotaExhausted)),
            (Lane::Go, 502, "gateway error", Some(UpstreamUnavailable)),
            (Lane::Go, 404, "not found", None),
        ] {
            assert_eq!(http(lane, status, body, Inference).map(|e| e.kind), expected, "{body}");
        }
    }

    #[test]
    fn oauth_sse_html_and_sanitization() {
        let invalid = br#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#;
        let error = classify_oauth_error(Lane::Google, StatusCode::BAD_REQUEST, invalid, &[]);
        assert_eq!((error.kind, error.source, error.http_status), (AuthExpired, OAuthRefresh, Some(400)));
        assert_eq!(classify_oauth_error(Lane::Google, StatusCode::BAD_GATEWAY, invalid, &[]).kind, UpstreamUnavailable);
        for (lane, event, body, kind) in [
            (Lane::Claude, "error", r#"{"type":"error","error":{"type":"overloaded_error"}}"#, UpstreamUnavailable),
            (Lane::Google, "message", GOOGLE_VERIFICATION, VerificationRequired),
            (Lane::Codex, "response.failed", r#"{"response":{"error":{"code":"insufficient_quota"}}}"#, BillingRequired),
            (Lane::Go, "error", "unexpected error", Other),
        ] {
            let error = classify_sse_error(lane, event, body.as_bytes(), &[]).unwrap();
            assert_eq!((error.kind, error.source), (kind, Stream));
        }
        assert!(classify_sse_error(Lane::Codex, "", br#"{"choices":[],"error":null}"#, &[]).is_none());
        let html = http(Lane::Go, 502, "<html>Cloudflare sk-private</html>", Inference).unwrap();
        assert_eq!(html.message, "Upstream returned a Cloudflare error page.");
        let message = sanitize(
            "  Bad\nBearer private-1, AIzaPrivate sk-private ya29.private key=private-2 token=private-3 access_token = private-4 \"refresh_token\": \"private-5\" https://user:secret@example.com/path?other=private-6#fragment",
        );
        assert!(!message.contains("private") && !message.contains("secret") && !message.contains('\n'), "{message}");
        assert!(message.contains("https://example.com"), "{message}");
        assert_eq!(sanitize(&"é".repeat(300)).chars().count(), 240);
    }
}
