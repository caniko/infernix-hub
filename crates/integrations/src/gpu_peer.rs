//! llama-swap as a GPU co-tenant: `GET /running` lists loaded models.

use async_trait::async_trait;
use domain::ports::{GpuPeer, PortError, PortResult};
use serde_json::Value;

pub struct LlamaSwapPeer {
    http: reqwest::Client,
}

impl LlamaSwapPeer {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

/// Busy when any listed model is not stopped. Unknown shapes count as busy.
pub fn running_busy(v: &Value) -> bool {
    match v.get("running").and_then(Value::as_array) {
        Some(models) => models.iter().any(|m| m.get("state").and_then(Value::as_str) != Some("stopped")),
        None => true,
    }
}

#[async_trait]
impl GpuPeer for LlamaSwapPeer {
    async fn busy(&self, running_url: &str) -> PortResult<bool> {
        let resp = self.http.get(running_url).send().await.map_err(|e| PortError::Unreachable(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(PortError::Rejected { status: resp.status().as_u16(), message: "llama-swap /running".into() });
        }
        let v: Value = resp.json().await.map_err(|e| PortError::Unreachable(e.to_string()))?;
        Ok(running_busy(&v))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn idle_only_when_every_model_is_stopped() {
        assert!(!super::running_busy(&json!({ "running": [] })));
        assert!(!super::running_busy(&json!({ "running": [{ "model": "a", "state": "stopped" }] })));
        assert!(super::running_busy(&json!({ "running": [{ "model": "a", "state": "ready" }] })));
        assert!(super::running_busy(&json!({ "unexpected": true })));
    }
}
