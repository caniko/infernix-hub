use std::time::Duration;

use domain::ports::{HumanizeInput, HumanizeOutput, HumanizeRequest, HumanizeResult, Humanizer, PortError, PortResult};
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;

pub struct HumanizerClient {
    http: Client,
    endpoint: Url,
    token: Option<String>,
}

impl HumanizerClient {
    pub fn new(base_url: &str, token: Option<String>, timeout: Option<Duration>) -> PortResult<Self> {
        let mut endpoint = Url::parse(base_url)
            .ok()
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
            .ok_or_else(|| PortError::Unreachable("invalid humanizer base URL".into()))?;
        endpoint.set_path(&format!("{}/api/v1/humanize", endpoint.path().trim_end_matches('/')));
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout.unwrap_or(Duration::from_secs(180)))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| super::transport_error(e.without_url(), false))?;
        Ok(Self { http, endpoint, token })
    }
    async fn send(&self, req: &(impl Serialize + Sync)) -> PortResult<HumanizeOutput> {
        let mut request = self.http.post(self.endpoint.clone()).json(req);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        // This only asks for text, not an external mutation. Never resubmit
        // from the draft path on a timeout or when the service is busy.
        let response = request.send().await.map_err(|e| super::transport_error(e.without_url(), false))?;
        let status = response.status();
        if status != StatusCode::OK {
            let message =
                if status == StatusCode::SERVICE_UNAVAILABLE { "humanizer busy or unavailable" } else { "humanizer request failed" };
            return Err(PortError::Rejected { status: status.as_u16(), message: message.into() });
        }
        response.json().await.map_err(|_| PortError::Rejected { status: 200, message: "invalid humanizer response".into() })
    }
}

#[async_trait::async_trait]
impl Humanizer for HumanizerClient {
    async fn humanize(&self, req: HumanizeInput) -> PortResult<HumanizeOutput> {
        self.send(&req).await
    }

    async fn humanize_with_voice(&self, req: &HumanizeRequest) -> PortResult<HumanizeResult> {
        if req.skip_judge {
            return Err(PortError::Unsupported("the Hub requires judged rewrites".into()));
        }
        self.send(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn humanizer_uses_the_synchronous_contract_and_handles_bad_responses() {
        for (status, body) in [
            (
                200,
                r#"{"humanized_text":"Hi @fan","paragraphs_processed":1,"retries_used":2,"fallback_paragraphs":[],"judge_verified":true,"paragraphs":[{"index":0}]}"#,
            ),
            (200, r#"{"humanized_text":"missing verification"}"#),
            (503, r#"{"error":"Server busy"}"#),
            (401, r#"{"error":"sensitive response body"}"#),
            (202, "{}"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client =
                HumanizerClient::new(&format!("http://{}/", listener.local_addr().unwrap()), Some("test-token".into()), None).unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert_eq!(line, "POST /api/v1/humanize HTTP/1.1\r\n");
                let (mut len, mut bearer) = (0, false);
                loop {
                    line.clear();
                    assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        match name.to_ascii_lowercase().as_str() {
                            "content-length" => len = value.trim().parse().unwrap(),
                            "authorization" => bearer = value.trim() == "Bearer test-token",
                            _ => {}
                        }
                    }
                }
                assert!(bearer);
                let mut request = vec![0; len];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&request).unwrap(),
                    json!({"text":"Hello @fan", "locked_facts":["@fan"], "max_chars":20, "best_of_n":2})
                );
                let response = format!(
                    "HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.get_mut().write_all(response.as_bytes()).await.unwrap();
            });
            let result = client
                .humanize(HumanizeInput {
                    text: "Hello @fan".into(),
                    locked_facts: vec!["@fan".into()],
                    max_chars: Some(20),
                    best_of_n: Some(2),
                })
                .await;
            server.await.unwrap();
            match (status, result) {
                (200, Ok(out)) if body.contains("paragraphs_processed") => {
                    assert_eq!(out.humanized_text, "Hi @fan");
                    assert_eq!((out.paragraphs_processed, out.retries_used), (1, 2));
                    assert!(out.judge_verified && out.fallback_paragraphs.is_empty());
                    assert_eq!(out.paragraphs, Some(vec![json!({"index":0})]));
                }
                (503, Err(PortError::Rejected { status: 503, message })) => assert!(message.contains("busy")),
                (expected, Err(PortError::Rejected { status, message })) if !body.contains("paragraphs_processed") => {
                    assert_eq!(status, expected);
                    assert!(!message.contains("sensitive response body"));
                }
                (_, other) => panic!("unexpected humanizer result: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn humanizer_transport_failure_is_a_port_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let client = HumanizerClient::new(&base, None, None).unwrap();
        assert!(matches!(
            client.humanize(HumanizeInput { text: "Hello".into(), ..Default::default() }).await,
            Err(PortError::Unreachable(_))
        ));
        assert!(matches!(
            client.humanize_with_voice(&HumanizeRequest { text: "Hello".into(), skip_judge: true, ..Default::default() }).await,
            Err(PortError::Unsupported(_))
        ));
    }

    #[test]
    fn humanizer_does_not_invent_a_host_for_a_malformed_base() {
        assert!(HumanizerClient::new("http://", None, None).is_err());
    }
}
