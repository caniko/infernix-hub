//! ComfyUI adapter (0.37.0), verified against the deployed source in
//! `/nix/store/88s35yl2nv1rkb8cqymmkgmrki7qhkv3-comfyui-0.37.0/share/comfyui/`
//! (`server.py`, `execution.py`, `comfy_execution/progress.py`).
//!
//! Verified behaviours this adapter relies on:
//! * `POST /prompt` honours a client-supplied `prompt_id` (validated UUID) and
//!   accepts `client_id`; response is `{prompt_id, number, node_errors}`.
//! * `GET /history/{id}` is `{}` when unknown, otherwise
//!   `{ "<id>": { prompt, outputs, status: {status_str, completed, messages} } }`.
//! * `GET /queue` is `{queue_running: [...], queue_pending: [...]}`; each item is
//!   a tuple whose element `[1]` is the prompt id.
//! * `POST /queue {"delete":[id]}` removes a pending prompt.
//! * `POST /interrupt {"prompt_id": id}` is a targeted interrupt; without
//!   `prompt_id` it is a global interrupt, which this adapter never sends.
//! * `/ws?clientId=…` sends JSON `{"type": event, "data": {...}}` frames plus
//!   binary preview frames (ignored).
//!
//! `/view` takes `filename`, `subfolder` and `type`; history output entries
//! carry the first two and a `type` of `output`/`temp`. `OutputFile.kind` keeps
//! the output category (`images`, `gifs`, `videos`, …) because the hub needs it
//! to classify assets, so `fetch` requests `type=output` — the category for
//! saved outputs.

use async_trait::async_trait;
use domain::ports::{CancelOutcome, Comfy, EngineLoad, OutputFile, PortError, PortResult, ProgressEvent, PromptOutcome};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::{read_bytes, read_json, transport_error, trim_base};

pub struct ComfyClient {
    http: reqwest::Client,
}

impl ComfyClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn queue(&self, base_url: &str) -> PortResult<Value> {
        let resp = self.http.get(format!("{base_url}/queue")).send().await.map_err(|e| transport_error(e, false))?;
        read_json(resp, true, false).await
    }
}

impl Default for ComfyClient {
    fn default() -> Self {
        Self::new(reqwest::Client::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePlace {
    Running,
    Pending(usize),
}

/// Locate a prompt id in `/queue`. Pending positions are the 0-based index in
/// `queue_pending`.
fn queue_place(queue: &Value, prompt_id: &str) -> Option<QueuePlace> {
    let running = queue.get("queue_running").and_then(Value::as_array);
    if running.is_some_and(|items| items.iter().any(|item| queue_item_id(item) == Some(prompt_id))) {
        return Some(QueuePlace::Running);
    }
    queue
        .get("queue_pending")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().position(|item| queue_item_id(item) == Some(prompt_id)).map(QueuePlace::Pending))
}

fn queue_item_id(item: &Value) -> Option<&str> {
    item.as_array()?.get(1)?.as_str()
}

/// Flatten every `outputs[node][kind][]` entry with a filename. Covers
/// `images`, `gifs`, `videos`, `audio`, `latents` and custom-node kinds without
/// hard-coding a list.
fn collect_outputs(outputs: Option<&Value>) -> Vec<OutputFile> {
    let mut files = Vec::new();
    let Some(outputs) = outputs.and_then(Value::as_object) else {
        return files;
    };
    for (node_id, node_outputs) in outputs {
        let Some(node_outputs) = node_outputs.as_object() else { continue };
        for (kind, entries) in node_outputs {
            let Some(entries) = entries.as_array() else { continue };
            for entry in entries {
                let Some(filename) = entry.get("filename").and_then(Value::as_str) else {
                    continue;
                };
                files.push(OutputFile {
                    node_id: node_id.clone(),
                    filename: filename.to_string(),
                    subfolder: entry.get("subfolder").and_then(Value::as_str).unwrap_or_default().to_string(),
                    kind: kind.clone(),
                });
            }
        }
    }
    files
}

/// Pull the exception message out of a history row's `status.messages`.
/// ComfyUI stores `(event, data)` tuples there, e.g.
/// `["execution_error", {"exception_message": "…"}]`.
fn history_error(entry: &Value) -> String {
    if let Some(messages) = entry.pointer("/status/messages").and_then(Value::as_array) {
        for message in messages {
            let Some(pair) = message.as_array() else { continue };
            if pair.first().and_then(Value::as_str) != Some("execution_error") {
                continue;
            }
            if let Some(text) = pair.get(1).and_then(|data| data.get("exception_message")).and_then(Value::as_str) {
                return text.to_string();
            }
        }
    }
    "comfyui execution failed".to_string()
}

fn history_outcome(entry: &Value) -> PromptOutcome {
    let status = entry.get("status");
    let status_str = status.and_then(|s| s.get("status_str")).and_then(Value::as_str);
    match status_str {
        Some("error") => PromptOutcome::Failed { message: history_error(entry) },
        Some("success") => PromptOutcome::Succeeded { outputs: collect_outputs(entry.get("outputs")) },
        _ => PromptOutcome::Unknown,
    }
}

/// Parse one `/ws` JSON frame. `None` covers frames the hub does not model
/// (`status`, `execution_cached`, `progress_state`, the terminating
/// `executing` with `node: null`, …). Binary preview frames never reach here.
fn parse_ws_message(raw: &str) -> Option<ProgressEvent> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let kind = value.get("type")?.as_str()?;
    let data = value.get("data")?;
    let prompt_id = data.get("prompt_id")?.as_str()?.to_string();
    match kind {
        "execution_start" => Some(ProgressEvent::Started { prompt_id }),
        "executing" => {
            let node = data.get("node")?.as_str()?;
            if node.is_empty() { None } else { Some(ProgressEvent::Executing { prompt_id, node: node.to_string() }) }
        }
        "progress" => Some(ProgressEvent::Progress {
            prompt_id,
            node: data.get("node").and_then(Value::as_str).map(str::to_string),
            value: as_u64(data.get("value")),
            max: as_u64(data.get("max")),
        }),
        "executed" => {
            let node = data.get("node")?.as_str()?.to_string();
            Some(ProgressEvent::NodeExecuted { prompt_id, node })
        }
        "execution_success" => Some(ProgressEvent::Finished { prompt_id }),
        "execution_error" => Some(ProgressEvent::Error {
            prompt_id,
            message: data.get("exception_message").and_then(Value::as_str).unwrap_or("comfyui execution error").to_string(),
        }),
        "execution_interrupted" => Some(ProgressEvent::Interrupted { prompt_id }),
        _ => None,
    }
}

fn as_u64(value: Option<&Value>) -> u64 {
    value.and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))).unwrap_or(0)
}

fn queue_ids(queue: &Value, key: &str) -> Vec<String> {
    queue
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(queue_item_id).map(String::from).collect())
        .unwrap_or_default()
}

#[async_trait]
impl Comfy for ComfyClient {
    async fn load(&self, base_url: &str) -> PortResult<EngineLoad> {
        let q = self.queue(&trim_base(base_url)).await?;
        Ok(EngineLoad { running: queue_ids(&q, "queue_running"), pending: queue_ids(&q, "queue_pending") })
    }

    async fn subscribe(&self, base_url: &str, client_id: &str, tx: tokio::sync::mpsc::Sender<ProgressEvent>) -> PortResult<()> {
        let mut url = reqwest::Url::parse(base_url).map_err(|e| PortError::Unreachable(format!("invalid comfy base url: {e}")))?;
        let scheme = match url.scheme() {
            "https" => "wss",
            _ => "ws",
        };
        url.set_scheme(scheme).map_err(|_| PortError::Unreachable("comfy base url cannot be a websocket url".into()))?;
        url.set_path("/ws");
        url.set_query(None);
        url.query_pairs_mut().append_pair("clientId", client_id);

        let (mut socket, _) =
            tokio_tungstenite::connect_async(url.as_str()).await.map_err(|e| PortError::Unreachable(format!("comfy ws connect: {e}")))?;
        while let Some(message) = socket.next().await {
            let message = message.map_err(|e| PortError::Unreachable(format!("comfy ws stream: {e}")))?;
            match message {
                Message::Text(text) => {
                    if let Some(event) = parse_ws_message(text.as_str()) {
                        // A dropped receiver means the worker stopped caring;
                        // closing the socket is the correct response.
                        if tx.send(event).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                Message::Close(_) => break,
                // Binary preview frames and ping/pong are display noise.
                _ => {}
            }
        }
        Ok(())
    }

    async fn outcome(&self, base_url: &str, prompt_id: &str) -> PortResult<PromptOutcome> {
        let base_url = trim_base(base_url);
        let resp = self.http.get(format!("{base_url}/history/{prompt_id}")).send().await.map_err(|e| transport_error(e, false))?;
        let history = read_json(resp, true, false).await?;
        if let Some(entry) = history.get(prompt_id) {
            return Ok(history_outcome(entry));
        }
        let queue = self.queue(&base_url).await?;
        match queue_place(&queue, prompt_id) {
            Some(QueuePlace::Running) => Ok(PromptOutcome::Pending { queue_position: None, running: true }),
            Some(QueuePlace::Pending(position)) => Ok(PromptOutcome::Pending { queue_position: Some(position), running: false }),
            None => Ok(PromptOutcome::Unknown),
        }
    }

    async fn fetch(&self, base_url: &str, file: &OutputFile) -> PortResult<Vec<u8>> {
        let resp = self
            .http
            .get(format!("{}/view", trim_base(base_url)))
            .query(&[("filename", file.filename.as_str()), ("subfolder", file.subfolder.as_str()), ("type", "output")])
            .send()
            .await
            .map_err(|e| transport_error(e, false))?;
        read_bytes(resp, true).await
    }

    async fn cancel(&self, base_url: &str, prompt_id: &str) -> PortResult<CancelOutcome> {
        let base_url = trim_base(base_url);
        let queue = self.queue(&base_url).await?;
        match queue_place(&queue, prompt_id) {
            Some(QueuePlace::Pending(_)) => {
                let resp = self
                    .http
                    .post(format!("{base_url}/queue"))
                    .json(&json!({ "delete": [prompt_id] }))
                    .send()
                    .await
                    .map_err(|e| transport_error(e, true))?;
                read_bytes(resp, false).await?;
                Ok(CancelOutcome::Dequeued)
            }
            Some(QueuePlace::Running) => {
                // Targeted interrupt only. Never send `{}`: that would stop an
                // unrelated prompt the user did not ask to cancel.
                let resp = self
                    .http
                    .post(format!("{base_url}/interrupt"))
                    .json(&json!({ "prompt_id": prompt_id }))
                    .send()
                    .await
                    .map_err(|e| transport_error(e, true))?;
                read_bytes(resp, false).await?;
                // The prompt may have finished between the queue read and the
                // interrupt; history is authoritative. A read failure here is
                // ambiguous: the interrupt was already sent.
                let hist = self
                    .http
                    .get(format!("{base_url}/history/{prompt_id}"))
                    .send()
                    .await
                    .map_err(|e| PortError::Ambiguous(format!("interrupt sent but history read failed: {e}")))?;
                let history = read_json(hist, true, false).await?;
                if history.get(prompt_id).is_some() { Ok(CancelOutcome::AlreadyFinished) } else { Ok(CancelOutcome::Interrupted) }
            }
            None => {
                let resp = self.http.get(format!("{base_url}/history/{prompt_id}")).send().await.map_err(|e| transport_error(e, false))?;
                let history = read_json(resp, true, false).await?;
                if history.get(prompt_id).is_some() {
                    Ok(CancelOutcome::AlreadyFinished)
                } else {
                    // Not queued, not running, not in history: nothing this
                    // adapter can safely act on.
                    Err(PortError::NotFound)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_success_collects_all_output_kinds() {
        let history: Value = serde_json::from_str(include_str!("testdata/comfy-history-success.json")).unwrap();
        let (prompt_id, entry) = history.as_object().unwrap().iter().next().unwrap();
        let PromptOutcome::Succeeded { mut outputs } = history_outcome(entry) else {
            panic!("expected success");
        };
        // serde_json's map order is not part of the contract; compare as a set.
        outputs.sort_by(|a, b| (a.node_id.as_str(), a.kind.as_str()).cmp(&(b.node_id.as_str(), b.kind.as_str())));
        assert_eq!(outputs.len(), 2);
        assert!(outputs.iter().any(|o| o.kind == "images" && o.filename == "Infernix_00001_.png" && o.subfolder.is_empty()));
        assert!(outputs.iter().any(|o| o.kind == "gifs" && o.filename == "clip_00001_.mp4" && o.subfolder == "video"));
        assert_eq!(prompt_id.len(), 36);
    }

    #[test]
    fn history_error_uses_the_exception_message() {
        let history: Value = serde_json::from_str(include_str!("testdata/comfy-history-error.json")).unwrap();
        let entry = history.as_object().unwrap().values().next().unwrap();
        assert_eq!(history_outcome(entry), PromptOutcome::Failed { message: "CUDA out of memory".into() });
    }

    #[test]
    fn queue_reports_running_and_pending_position() {
        let queue: Value = serde_json::from_str(include_str!("testdata/comfy-queue.json")).unwrap();
        assert_eq!(queue_place(&queue, "11111111-1111-4111-8111-111111111111"), Some(QueuePlace::Running));
        assert_eq!(queue_place(&queue, "22222222-2222-4222-8222-222222222222"), Some(QueuePlace::Pending(0)));
        assert_eq!(queue_place(&queue, "33333333-3333-4333-8333-333333333333"), Some(QueuePlace::Pending(1)));
        assert_eq!(queue_place(&queue, "44444444-4444-4444-8444-444444444444"), None);
    }

    #[test]
    fn ws_frames_map_to_progress_events() {
        let frames: Vec<Value> = serde_json::from_str(include_str!("testdata/comfy-ws-frames.json")).unwrap();
        let events: Vec<ProgressEvent> = frames.iter().filter_map(|frame| parse_ws_message(&frame.to_string())).collect();
        assert_eq!(
            events,
            vec![
                ProgressEvent::Started { prompt_id: "11111111-1111-4111-8111-111111111111".into() },
                ProgressEvent::Executing { prompt_id: "11111111-1111-4111-8111-111111111111".into(), node: "3".into() },
                ProgressEvent::Progress {
                    prompt_id: "11111111-1111-4111-8111-111111111111".into(),
                    node: Some("3".into()),
                    value: 4,
                    max: 20
                },
                ProgressEvent::NodeExecuted { prompt_id: "11111111-1111-4111-8111-111111111111".into(), node: "9".into() },
                ProgressEvent::Finished { prompt_id: "11111111-1111-4111-8111-111111111111".into() },
                ProgressEvent::Error { prompt_id: "22222222-2222-4222-8222-222222222222".into(), message: "boom".into() },
                ProgressEvent::Interrupted { prompt_id: "33333333-3333-4333-8333-333333333333".into() },
            ]
        );
    }

    #[test]
    fn ws_ignores_status_and_terminating_executing_frames() {
        assert!(parse_ws_message(r#"{"type":"status","data":{"status":{"exec_info":{"queue_remaining":0}}}}"#).is_none());
        assert!(
            parse_ws_message(r#"{"type":"executing","data":{"node":null,"prompt_id":"11111111-1111-4111-8111-111111111111"}}"#).is_none()
        );
    }

    /// Read-only probe of the deployed ComfyUI. Run explicitly:
    /// `cargo test -p hub-integrations live_probe -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "read-only probe of the deployed ComfyUI instance"]
    async fn live_probe_system_stats_and_queue() {
        let base = std::env::var("COMFY_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8188".to_string());
        let http = reqwest::Client::new();
        let stats: Value = http
            .get(format!("{}/system_stats", trim_base(&base)))
            .send()
            .await
            .expect("GET /system_stats")
            .json()
            .await
            .expect("system_stats json");
        assert!(stats.get("system").is_some(), "unexpected system_stats: {stats}");
        let queue: Value =
            http.get(format!("{}/queue", trim_base(&base))).send().await.expect("GET /queue").json().await.expect("queue json");
        assert!(queue.get("queue_running").is_some(), "unexpected queue: {queue}");
        assert!(queue.get("queue_pending").is_some(), "unexpected queue: {queue}");
    }
}
