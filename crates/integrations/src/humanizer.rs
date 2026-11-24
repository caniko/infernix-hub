//! HTTP client for the synchronous humanizer-rs rewrite endpoint.

pub use crate::live::HumanizerClient as HttpHumanizer;

#[cfg(test)]
mod tests {
    use domain::ports::{HumanizeRequest, HumanizeResult, HumanizerVoiceExemplar, SpokenConstraints, VoiceOptions};
    use serde_json::json;

    #[test]
    fn humanize_wire_matches_upstream_document_and_voice_fields() {
        let mut request = HumanizeRequest {
            text: "Raw reply".into(),
            locked_facts: vec!["Host".into()],
            max_chars: Some(140),
            best_of_n: Some(4),
            allow_q3_fallback: true,
            skip_judge: false,
            voice: Some(VoiceOptions {
                mode: contracts::VoiceMode::Spoken,
                avoid: vec!["Catchphrase".into()],
                exemplars: vec![HumanizerVoiceExemplar { draft: "Raw".into(), rewrite: "In voice".into() }],
                reference_lines: vec!["Reference".into()],
                target_similarity_band: Some([0.25, 0.5]),
                spoken: Some(SpokenConstraints { min_words: Some(3), ..Default::default() }),
            }),
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "text":"Raw reply", "locked_facts":["Host"], "max_chars":140, "best_of_n":4, "allow_q3_fallback":true, "skip_judge":false,
                "voice": {
                    "mode":"spoken", "avoid":["Catchphrase"], "exemplars":[{"draft":"Raw","rewrite":"In voice"}],
                    "reference_lines":["Reference"], "target_similarity_band":[0.25,0.5],
                    "spoken":{"max_sentence_words":28,"min_words":3,"max_words":null,"forbid_parentheses":true,"forbid_urls":true}
                }
            })
        );
        request.max_chars = None;
        assert!(serde_json::to_value(request).unwrap().get("max_chars").is_none());
        let result: HumanizeResult = serde_json::from_value(json!({
            "humanized_text":"Final reply", "paragraphs_processed":1, "retries_used":2, "fallback_paragraphs":[0], "judge_verified":true
        }))
        .unwrap();
        assert_eq!(result.humanized_text, "Final reply");
        assert_eq!(result.paragraphs_processed, 1);
        assert_eq!(result.retries_used, 2);
        assert!(result.judge_verified);
    }
}
