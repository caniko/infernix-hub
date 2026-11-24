use std::collections::HashSet;

use contracts::{HumanizeInput, HumanizePreview};

use crate::{
    Hub,
    ports::{HumanizeRequest, PortError, VoiceOptions},
};

impl Hub {
    /// Try a rewrite without touching the database. A failed check always
    /// selects the exact original, even if it exceeds the requested limit.
    pub async fn humanize_draft(&self, req: HumanizeInput) -> HumanizePreview {
        self.humanize_draft_with_voice(req, None).await
    }

    pub(crate) async fn humanize_draft_with_voice(&self, mut req: HumanizeInput, voice: Option<VoiceOptions>) -> HumanizePreview {
        let mut preview = HumanizePreview {
            applied: false,
            source_text: req.text.clone(),
            humanized_text: req.text.clone(),
            judge_verified: false,
            fallback_paragraphs: Vec::new(),
            paragraphs: None,
            reason: None,
        };
        if req.text.trim().is_empty() {
            preview.reason = Some("text must not be empty".into());
            return preview;
        }
        if req.best_of_n.is_some_and(|n| !(1..=64).contains(&n)) {
            preview.reason = Some("best_of_n must be between 1 and 64".into());
            return preview;
        }
        if req.max_chars == Some(0) {
            preview.reason = Some("max_chars must be greater than zero".into());
            return preview;
        }
        let max_chars = req.max_chars;
        let facts = merge_facts(&req.text, &mut req.locked_facts);
        let result = if let Some(voice) = voice {
            self.humanizer
                .humanize_with_voice(&HumanizeRequest {
                    text: req.text,
                    locked_facts: req.locked_facts,
                    max_chars,
                    best_of_n: req.best_of_n.map(|n| n as u8),
                    voice: Some(voice),
                    ..Default::default()
                })
                .await
        } else {
            self.humanizer.humanize(req).await
        };
        match result {
            Ok(out) => {
                let reason = if !out.judge_verified {
                    Some("rewrite was not verified by the judge")
                } else if !out.fallback_paragraphs.is_empty() {
                    Some("rewrite used fallback paragraphs")
                } else if out.humanized_text.trim().is_empty() {
                    Some("rewrite was empty")
                } else if max_chars.is_some_and(|limit| out.humanized_text.chars().count() > limit) {
                    Some("rewrite exceeded max_chars")
                } else if facts.iter().any(|fact| preview.source_text.contains(fact) && !out.humanized_text.contains(fact)) {
                    Some("rewrite dropped a locked fact")
                } else {
                    None
                };
                preview.judge_verified = out.judge_verified;
                preview.fallback_paragraphs = out.fallback_paragraphs;
                preview.paragraphs = out.paragraphs;
                preview.reason = reason.map(str::to_owned);
                if reason.is_none() {
                    preview.applied = true;
                    preview.humanized_text = out.humanized_text;
                }
            }
            Err(error) => {
                preview.reason = Some(match error {
                    PortError::Unsupported(_) => "humanizer integration is disabled".into(),
                    PortError::Rejected { status: 503, .. } => "humanizer is busy or temporarily unavailable (HTTP 503)".into(),
                    PortError::Rejected { status: 200, .. } => "humanizer returned an invalid response".into(),
                    PortError::Rejected { status, .. } => format!("humanizer request failed (HTTP {status})"),
                    _ => "humanizer backend unavailable".into(),
                });
            }
        }
        preview
    }
}

fn merge_facts(text: &str, facts: &mut Vec<String>) -> HashSet<String> {
    facts.extend(social_facts(text).into_iter().map(str::to_owned));
    let mut seen = HashSet::new();
    facts.retain(|fact| !fact.trim().is_empty() && seen.insert(fact.clone()));
    seen
}

fn social_facts(text: &str) -> Vec<&str> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut facts = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let before = text[..text.len() - rest.len()].chars().next_back();
        if !before.is_some_and(|c| word(c) || c == '@') {
            let fact = if ["https://", "http://"].iter().any(|p| rest.get(..p.len()).is_some_and(|s| s.eq_ignore_ascii_case(p))) {
                let (mut parens, mut brackets) = (0, 0);
                let end = rest
                    .find(|c: char| match c {
                        '(' => {
                            parens += 1;
                            false
                        }
                        ')' if parens > 0 => {
                            parens -= 1;
                            false
                        }
                        '[' => {
                            brackets += 1;
                            false
                        }
                        ']' if brackets > 0 => {
                            brackets -= 1;
                            false
                        }
                        _ => c.is_whitespace() || "<>\"'`)]".contains(c),
                    })
                    .unwrap_or(rest.len());
                rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?'])
            } else if matches!(c, '@' | '#') && rest[1..].chars().next().is_some_and(word) {
                let end = rest[1..].find(|ch| !(word(ch) || (c == '@' && ".-@".contains(ch)))).map_or(rest.len(), |i| i + 1);
                rest[..end].trim_end_matches(['.', '-', '@'])
            } else {
                ""
            };
            if !fact.is_empty() {
                facts.push(fact);
                rest = &rest[fact.len()..];
                continue;
            }
        }
        rest = &rest[c.len_utf8()..];
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_caller_facts_with_social_tokens_without_duplicates() {
        let mut facts = vec!["Order 42".into(), "@alice".into()];
        let merged = merge_facts(
            "Hi (@alice), @alice! @fan@example.org and @alice.bsky.social. #Énergie [docs](https://example.org/A_(B)?q=@foo#bar). \
             <HTTP://example.net/x>, http://[::1]:3000/ , mail alice@example.com and #one,#two.",
            &mut facts,
        );
        assert_eq!(
            facts,
            [
                "Order 42",
                "@alice",
                "@fan@example.org",
                "@alice.bsky.social",
                "#Énergie",
                "https://example.org/A_(B)?q=@foo#bar",
                "HTTP://example.net/x",
                "http://[::1]:3000/",
                "#one",
                "#two",
            ]
        );
        assert_eq!(merged.len(), facts.len());
    }
}
