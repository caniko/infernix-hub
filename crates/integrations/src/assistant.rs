//! Creative assistant over an OpenAI-compatible local endpoint (llama-swap on
//! atlas: `http://127.0.0.1:8013/v1`). Canon suggestions come back as a
//! fenced ```canon JSON block and are recorded as proposals only.

use async_trait::async_trait;
use domain::ports::{Assistant, AssistantReply, AssistantTurn, PortError, PortResult};
use serde_json::{Value, json};

pub struct OpenAiAssistant {
    http: reqwest::Client,
    base: String,
    model: String,
}

impl OpenAiAssistant {
    pub fn new(base: &str, model: &str) -> Self {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(180)).build().expect("reqwest client");
        Self { http, base: base.trim_end_matches('/').into(), model: model.into() }
    }
}

const SYSTEM: &str = "You are a creative collaborator inside a content operations hub. \
Use the persona's accepted canon and the brief. Be concrete and brief. \
If you suggest changing the persona's canon, append exactly one fenced block:\n```canon\n{\"voice\":\"...\",\"rules\":[...],\"avoid\":[...],\"world\":[...]}\n```\n\
The block is a proposal for human review; never claim it is applied.";

pub fn parse_canon_block(text: &str) -> (String, Option<contracts::CanonBody>) {
    let Some(start) = text.find("```canon") else { return (text.trim().to_string(), None) };
    let rest = &text[start + 8..];
    let Some(end) = rest.find("```") else { return (text.trim().to_string(), None) };
    let body = serde_json::from_str(rest[..end].trim()).ok();
    (format!("{}{}", &text[..start], &rest[end + 3..]).trim().to_string(), body)
}

#[async_trait]
impl Assistant for OpenAiAssistant {
    async fn reply(&self, context: &Value, turns: &[AssistantTurn]) -> PortResult<AssistantReply> {
        let mut messages = vec![json!({ "role": "system", "content": format!("{SYSTEM}\n\nContext:\n{context}") })];
        for t in turns.iter().filter(|t| t.role != "system") {
            let role = if t.role == "assistant" { "assistant" } else { "user" };
            messages.push(json!({ "role": role, "content": t.body }));
        }
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base))
            .json(&json!({ "model": self.model, "messages": messages, "temperature": 0.7 }))
            .send()
            .await
            .map_err(|e| PortError::Unreachable(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| PortError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            return Err(PortError::Rejected { status: status.as_u16(), message: body.to_string() });
        }
        let text = body["choices"][0]["message"]["content"].as_str().unwrap_or_default();
        let (body_text, canon_suggestion) = parse_canon_block(text);
        Ok(AssistantReply { body: body_text, canon_suggestion, model: self.model.clone() })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn canon_block_is_split_from_reply() {
        let (text, canon) = super::parse_canon_block("Try this.\n```canon\n{\"voice\":\"dry\",\"rules\":[\"no emoji\"]}\n```\nOk?");
        assert_eq!(text, "Try this.\n\nOk?");
        assert_eq!(canon.unwrap().rules, vec!["no emoji"]);
        assert!(super::parse_canon_block("plain").1.is_none());
    }
}
