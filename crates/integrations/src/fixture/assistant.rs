//! Fixture creative assistant: deterministic text and canon suggestions that
//! the hub records as proposals only.

use async_trait::async_trait;
use contracts::CanonBody;
use domain::ports::*;
use serde_json::Value;

/// Deterministic creative assistant. A message containing `rule:` yields a
/// canon suggestion (which the hub records as a proposal only).
pub struct FixtureAssistant;

#[async_trait]
impl Assistant for FixtureAssistant {
    async fn reply(&self, context: &Value, turns: &[AssistantTurn]) -> PortResult<AssistantReply> {
        let last = turns.iter().rev().find(|t| t.role == "operator").map(|t| t.body.clone()).unwrap_or_default();
        let persona = context["persona"]["name"].as_str().unwrap_or("the persona");
        let canon_suggestion = last.split_once("rule:").map(|(_, rule)| {
            let mut body: CanonBody = serde_json::from_value(context["persona"]["canon"]["body"].clone()).unwrap_or_default();
            if body.voice.is_empty() {
                body.voice = format!("{persona} voice");
            }
            body.rules.push(rule.trim().to_string());
            body
        });
        let body = match &canon_suggestion {
            Some(_) => format!("[fixture] I drafted a canon proposal for {persona}. It stays pending until you review it."),
            None => format!(
                "[fixture] For {persona}: consider leading with the strongest visual, then one concrete detail. You said: \"{last}\""
            ),
        };
        Ok(AssistantReply { body, canon_suggestion, model: "fixture-assistant".into() })
    }
}
