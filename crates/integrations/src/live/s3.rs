//! One S3 `GetObject`, path-style, signed with AWS Signature V4.
//!
//! Enough S3 for status feeds on Ceph RGW without an SDK: no query string, no
//! payload, a header-signed request. The credentials travel only in the
//! `Authorization` header's HMAC; errors never carry the URL or headers.

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::SecretString;

/// SHA-256 of the empty payload.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, Clone)]
pub struct S3Object {
    /// `https://host[:port]`, no path.
    pub endpoint: reqwest::Url,
    pub region: String,
    pub bucket: String,
    pub key: String,
}

#[derive(Debug)]
pub struct S3Credentials {
    pub access_key_id: String,
    pub secret_access_key: SecretString,
}

/// Fetch the object, reading at most `max_bytes`. The error is safe to log.
pub async fn get_object(http: &reqwest::Client, object: &S3Object, creds: &S3Credentials, max_bytes: usize) -> Result<Vec<u8>, String> {
    let path = format!("/{}/{}", uri_encode(&object.bucket, false), uri_encode(&object.key, true));
    let host = match (object.endpoint.host_str(), object.endpoint.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => return Err("endpoint has no host".into()),
    };
    let mut url = object.endpoint.clone();
    url.set_path(&path);
    let amz_date = amz_timestamp(Utc::now());
    let headers = [("host", host.as_str()), ("x-amz-content-sha256", EMPTY_SHA256), ("x-amz-date", amz_date.as_str())];
    let authorization = authorization(creds, &object.region, &amz_date, &canonical_request("GET", &path, &headers));
    let mut resp = http
        .get(url)
        .header("x-amz-content-sha256", EMPTY_SHA256)
        .header("x-amz-date", &amz_date)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e.without_url()))?;
    let status = resp.status();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("reading the response: {}", e.without_url()))? {
        if body.len() + chunk.len() > max_bytes {
            if status.is_success() {
                return Err(format!("object is larger than {max_bytes} bytes"));
            }
            break;
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        // S3 errors are `<Error><Code>AccessDenied</Code>…`; the code is the useful part.
        return Err(match error_code(&body) {
            Some(code) => format!("HTTP {} {code}", status.as_u16()),
            None => format!("HTTP {}", status.as_u16()),
        });
    }
    Ok(body)
}

fn error_code(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let start = text.find("<Code>")? + "<Code>".len();
    let code = &text[start..start + text[start..].find("</Code>")?];
    (!code.is_empty() && code.len() <= 64 && code.chars().all(|c| c.is_ascii_alphanumeric())).then(|| code.to_string())
}

/// SigV4 URI encoding: unreserved characters pass, `/` too inside a key.
fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') || (keep_slash && byte == b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `headers` are lowercase, sorted, and include `x-amz-content-sha256`.
fn canonical_request(method: &str, path: &str, headers: &[(&str, &str)]) -> String {
    let lines: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let payload = headers.iter().find(|(k, _)| *k == "x-amz-content-sha256").map_or(EMPTY_SHA256, |(_, v)| v);
    format!("{method}\n{path}\n\n{lines}\n{}\n{payload}", signed.join(";"))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn authorization(creds: &S3Credentials, region: &str, amz_date: &str, canonical: &str) -> String {
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let mut key = hmac(format!("AWS4{}", creds.secret_access_key.expose()).as_bytes(), date);
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part);
    }
    let signed = canonical.lines().rev().nth(1).unwrap_or_default();
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={}",
        creds.access_key_id,
        hex::encode(hmac(&key, &to_sign))
    )
}

fn amz_timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's published S3 SigV4 example: GET Object (`/test.txt`, with a Range header).
    #[test]
    fn signature_matches_the_aws_example() {
        let creds = S3Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: SecretString::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
        };
        let at = DateTime::parse_from_rfc3339("2013-05-24T00:00:00Z").unwrap().with_timezone(&Utc);
        let date = amz_timestamp(at);
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", date.as_str()),
        ];
        let auth = authorization(&creds, "us-east-1", &date, &canonical_request("GET", "/test.txt", &headers));
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(uri_encode("a b/c+d~", true), "a%20b/c%2Bd~");
        assert_eq!(error_code(b"<Error><Code>NoSuchKey</Code></Error>").as_deref(), Some("NoSuchKey"));
        assert_eq!(error_code(b"<Code><script></Code>"), None);
    }
}
