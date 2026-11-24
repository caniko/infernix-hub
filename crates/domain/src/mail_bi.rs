//! Deterministic, local-only mail analysis. Message contents are untrusted data.

use chrono::{DateTime, Utc};
use contracts::{MailAnalysisView, MailBiOverview, MailEntities, MailMessageIn, MonetaryAmount};
use std::fmt::Write;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transactional_mail_is_signal_even_from_a_bulk_no_reply_sender() {
        let mut message = MailMessageIn {
            from_addr: "no-reply@billing.example.com".into(),
            from_name: " Example Services ".into(),
            subject: "Invoice INV-104".into(),
            body_text: "Total €1.234,56. Please pay by 2026-12-03.\nPrevious credit $12.50.".into(),
            ..Default::default()
        };
        message.raw_headers.insert("Precedence".into(), "bulk".into());
        message.raw_headers.insert("List-Unsubscribe".into(), "<mailto:unsubscribe@example.com>".into());
        let analysis = analyze_message(&message, DateTime::UNIX_EPOCH);
        assert_eq!(analysis.classification, "invoice");
        assert!(!analysis.is_junk);
        assert_eq!(analysis.entities.vendor.as_deref(), Some("example services"));
        assert_eq!(analysis.entities.invoice_ids, ["INV-104"]);
        assert_eq!(analysis.entities.deadlines, ["2026-12-03"]);
        assert_eq!(
            analysis.entities.amounts.iter().map(|a| (a.amount.as_str(), a.currency.as_str())).collect::<Vec<_>>(),
            [("1234.56", "EUR"), ("12.50", "USD")]
        );
        assert_eq!(analysis.action_items, ["Please pay by 2026-12-03."]);
        message.subject = "Your receipt".into();
        assert_eq!(analyze_message(&message, DateTime::UNIX_EPOCH).classification, "receipt");
    }

    #[test]
    fn separates_newsletter_spam_and_action_requests() {
        let mut message = MailMessageIn {
            subject: "Weekly newsletter".into(),
            from_addr: "newsletter@example.org".into(),
            body_text: "Unsubscribe at any time.".into(),
            ..Default::default()
        };
        message.raw_headers.insert("list-unsubscribe".into(), "present".into());
        let analysis = analyze_message(&message, DateTime::UNIX_EPOCH);
        assert!(analysis.is_junk && (0.65..=1.).contains(&analysis.junk_score));
        assert_eq!(analysis.classification, "newsletter");
        message.raw_headers.insert("X-Spam-Flag".into(), "YES".into());
        assert_eq!(analyze_message(&message, DateTime::UNIX_EPOCH).classification, "junk");
        let action = analyze_message(
            &MailMessageIn { subject: "Approval needed".into(), body_text: "Please approve the proposal.".into(), ..Default::default() },
            DateTime::UNIX_EPOCH,
        );
        assert_eq!(action.classification, "action_required");
        assert!(!action.is_junk);
    }

    #[test]
    fn truncation_respects_utf8_and_counts_snippet_characters() {
        let text = "€".repeat(BODY_LIMIT / 3 + 10);
        assert_eq!(bounded_body(&text).len(), BODY_LIMIT - BODY_LIMIT % 3);
        assert_eq!(bounded_snippet("", &text).chars().count(), 300);
    }
}

const BODY_LIMIT: usize = 256 * 1024;

pub(crate) fn bounded_body(body: &str) -> &str {
    &body[..body.floor_char_boundary(BODY_LIMIT)]
}

pub(crate) fn bounded_snippet(snippet: &str, body: &str) -> String {
    let text = if snippet.is_empty() { body } else { snippet };
    text.chars().take(300).collect()
}

pub(crate) fn sender_domain(address: &str) -> String {
    address.trim().rsplit_once('@').map(|(_, domain)| domain.trim_end_matches('>').to_ascii_lowercase()).unwrap_or_default()
}

pub(crate) const CLASSES: &[&str] =
    &["business", "financial", "invoice", "receipt", "legal", "action_required", "personal", "newsletter", "promo", "junk", "automated"];

pub fn analyze_message(message: &MailMessageIn, analyzed_at: DateTime<Utc>) -> MailAnalysisView {
    let subject = message.subject.to_lowercase();
    let body = bounded_body(&message.body_text);
    let lower = body.to_lowercase();
    let sender = message.from_addr.to_ascii_lowercase();
    let header = |key: &str| {
        message
            .raw_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.trim().to_ascii_lowercase())
            .unwrap_or_default()
    };
    let has = |text: &str, cues: &[&str]| cues.iter().any(|cue| text.contains(cue));
    let news = has(&subject, &["newsletter", "weekly roundup", "weekly digest"]) || sender.contains("newsletter");
    let promo = has(&subject, &["% off", "sale ends", "special offer", "limited time"]) || sender.contains("marketing");
    let automated = matches!(header("auto-submitted").as_str(), "auto-generated" | "auto-replied")
        || has(&sender, &["noreply", "no-reply", "notifications@"]);
    let spam = matches!(header("x-spam-flag").as_str(), "yes" | "true");
    let mut score = 0.0_f64;
    for (matched, weight) in [
        (spam, 1.0),
        (!header("list-unsubscribe").is_empty(), 0.40),
        (matches!(header("precedence").as_str(), "bulk" | "list" | "junk"), 0.22),
        (has(&header("x-mailer"), &["mailchimp", "sendy", "campaign monitor", "bulk"]), 0.22),
        (automated, 0.15),
        (news, 0.32),
        (promo, 0.40),
        (lower.contains("unsubscribe"), 0.16),
    ] {
        if matched {
            score += weight;
        }
    }

    let action_cues =
        ["please ", "action required", "approval needed", "approve ", "payment due", "pay by", "deadline", "respond by", "due by"];
    let action_items: Vec<_> = sentences(body)
        .filter(|sentence| has(&sentence.to_lowercase(), &action_cues))
        .take(12)
        .map(|sentence| sentence.chars().take(300).collect::<String>())
        .collect();
    let amounts = extract_amounts(&message.subject, body);
    let signal_class = if has(&subject, &["receipt", "payment confirmation", "payment received", "order confirmation"]) {
        Some("receipt")
    } else if subject.contains("invoice") || has(&lower, &["invoice number", "invoice #", "invoice no."]) {
        Some("invoice")
    } else if has(&subject, &["contract", "legal notice", "agreement", "terms of service"]) {
        Some("legal")
    } else if has(&subject, &action_cues) || (!action_items.is_empty() && !news && !promo) {
        Some("action_required")
    } else if has(&subject, &["statement", "payment", "tax", "refund", "balance"]) || (!amounts.is_empty() && !promo && !news) {
        Some("financial")
    } else if has(&subject, &["proposal", "project", "meeting", "quote", "business"]) {
        Some("business")
    } else {
        None
    };
    if signal_class.is_some() && !spam {
        score = (score - 0.65).max(0.0);
    }
    let junk_score = score.min(1.);
    let is_junk = junk_score >= 0.65;
    let classification = if spam {
        "junk"
    } else if let Some(class) = signal_class {
        class
    } else if news {
        "newsletter"
    } else if promo {
        "promo"
    } else if is_junk {
        "junk"
    } else if automated {
        "automated"
    } else {
        "personal"
    };
    let priority = if is_junk {
        "noise"
    } else if has(&subject, &["urgent", "immediate action"]) {
        "urgent"
    } else if !action_items.is_empty() || classification == "action_required" {
        "high"
    } else if automated && signal_class.is_none() {
        "low"
    } else {
        "normal"
    };
    let domain = sender_domain(&message.from_addr);
    let display_name = message.from_name.split_whitespace().collect::<Vec<_>>().join(" ");
    let vendor = if display_name.is_empty() { domain.trim_start_matches("www.").to_owned() } else { display_name.to_lowercase() };
    let words = || message.subject.split_whitespace().chain(body.split_whitespace());
    let mut invoice_ids = Vec::new();
    for (previous, token) in words().zip(words().skip(1)) {
        let id = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
        if matches!(
            previous.trim_end_matches(['#', ':']).to_ascii_lowercase().as_str(),
            "invoice" | "order" | "invoice-id" | "order-id" | "number" | "no."
        ) && id.len() <= 64
            && id.bytes().any(|c| c.is_ascii_digit())
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'/'))
            && !invoice_ids.iter().any(|s| s == id)
        {
            invoice_ids.push(id.to_owned());
            if invoice_ids.len() == 12 {
                break;
            }
        }
    }
    let mut deadlines = Vec::new();
    for date in action_items.iter().flat_map(|item| item.split_whitespace()) {
        let date = date.trim_matches(|ch: char| !ch.is_ascii_digit());
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok() && !deadlines.iter().any(|d| d == date) {
            deadlines.push(date.to_owned());
        }
    }
    let topics = ["invoice", "payment", "tax", "contract", "meeting", "project", "approval", "shipping"]
        .into_iter()
        .filter(|topic| subject.contains(topic) || lower.contains(topic))
        .map(str::to_owned)
        .collect();
    MailAnalysisView {
        classification: classification.into(),
        is_junk,
        junk_score,
        priority: priority.into(),
        summary: bounded_snippet(&message.snippet, if body.is_empty() { &message.subject } else { body }),
        entities: MailEntities {
            vendor: (!vendor.is_empty()).then_some(vendor),
            amounts,
            invoice_ids,
            deadlines,
            people: (!display_name.is_empty() && matches!(classification, "personal" | "action_required" | "business"))
                .then_some(display_name)
                .into_iter()
                .collect(),
            orgs: (!domain.is_empty()).then_some(domain).into_iter().collect(),
        },
        action_items,
        topics,
        user_override_classification: None,
        user_override_junk: None,
        analyzer_kind: "heuristic".into(),
        analyzer_model: None,
        analyzed_at,
    }
}

fn sentences(text: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    text.char_indices().filter_map(move |(i, ch)| {
        let end = i + ch.len_utf8();
        if ch == '\n' || (matches!(ch, '.' | '?' | '!') && text[end..].starts_with(char::is_whitespace)) || end == text.len() {
            let sentence = text[start..end].trim();
            start = end;
            (!sentence.is_empty()).then_some(sentence)
        } else {
            None
        }
    })
}

fn currency(token: &str) -> Option<&'static str> {
    match token.to_ascii_uppercase().as_str() {
        "EUR" | "€" => Some("EUR"),
        "USD" | "$" => Some("USD"),
        "GBP" | "£" => Some("GBP"),
        "CHF" => Some("CHF"),
        _ => None,
    }
}

fn normalized_amount(raw: &str) -> Option<String> {
    let raw = raw.trim_end_matches([',', '.']);
    let digits = raw.strip_prefix(['-', '+']).unwrap_or(raw);
    if !digits.starts_with(|c: char| c.is_ascii_digit()) || !digits.ends_with(|c: char| c.is_ascii_digit()) || digits.len() > 32 {
        return None;
    }
    let decimal = digits.rfind([',', '.']).filter(|i| digits.len() - i <= 3);
    let (whole, fraction) = decimal.map_or((digits, None), |i| (&digits[..i], Some(&digits[i + 1..])));
    let separator = whole.find([',', '.', '\'', '’']).and_then(|i| whole[i..].chars().next());
    if let Some(separator) = separator {
        let mut groups = whole.split(separator);
        if !groups.next().is_some_and(|first| (1..=3).contains(&first.len()) && first.bytes().all(|c| c.is_ascii_digit()))
            || !groups.all(|group| group.len() == 3 && group.bytes().all(|c| c.is_ascii_digit()))
        {
            return None;
        }
    } else if !whole.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole: String = whole.bytes().filter(u8::is_ascii_digit).map(char::from).collect();
    let mut amount = if raw.starts_with('-') { "-".to_owned() } else { String::new() };
    let whole = whole.trim_start_matches('0');
    amount.push_str(if whole.is_empty() { "0" } else { whole });
    if let Some(fraction) = fraction {
        if !fraction.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        amount.push('.');
        amount.push_str(fraction);
    }
    Some(amount)
}

fn extract_amounts(subject: &str, body: &str) -> Vec<MonetaryAmount> {
    let mut amounts = Vec::new();
    for text in [subject, body] {
        // Split currency symbols from adjacent digits without losing the input span.
        let mut tokens = Vec::new();
        let mut start = None;
        for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
            if c.is_whitespace() || matches!(c, '€' | '$' | '£' | '(' | ')' | ':' | ';' | '=' | '[' | ']') {
                if let Some(begin) = start.take() {
                    tokens.push((begin, i));
                }
                if matches!(c, '€' | '$' | '£') {
                    tokens.push((i, i + c.len_utf8()));
                }
            } else {
                start.get_or_insert(i);
            }
        }
        let mut i = 0;
        while i + 1 < tokens.len() {
            let (a, b) = tokens[i];
            let (c, d) = tokens[i + 1];
            let left = text[a..b].trim_end_matches([',', '.']);
            let right = text[c..d].trim_end_matches([',', '.']);
            let pair = if text[b..c].chars().all(char::is_whitespace) {
                currency(left).zip(normalized_amount(right)).or_else(|| currency(right).zip(normalized_amount(left)))
            } else {
                None
            };
            if let Some((currency, amount)) = pair {
                let raw = text[a..d].trim_end_matches([',', '.']).to_owned();
                if !amounts.iter().any(|item: &MonetaryAmount| item.currency == currency && item.amount == amount) {
                    amounts.push(MonetaryAmount { amount, currency: currency.into(), raw });
                    if amounts.len() == 24 {
                        return amounts;
                    }
                }
                i += 1;
            }
            i += 1;
        }
    }
    amounts
}

pub fn executive_digest(overview: &MailBiOverview) -> String {
    let mut output = format!(
        "# Mail intelligence digest\n\n{} messages · {} signal · {} junk · {} unread\n",
        overview.total_count, overview.signal_count, overview.junk_count, overview.unread_count,
    );
    // Keep untrusted message text literal in Markdown; never produce links/HTML.
    let literal = |text: &str| -> String {
        text.chars()
            .take(300)
            .flat_map(|c| if c.is_ascii_punctuation() { [Some('\\'), Some(c)] } else { [None, Some(if c.is_control() { ' ' } else { c })] })
            .flatten()
            .collect()
    };
    output.push_str("\n## Recurring vendors\n");
    for vendor in &overview.recurring_vendors {
        let _ = writeln!(output, "- {}: {} messages", literal(&vendor.vendor), vendor.message_count);
    }
    output.push_str("\n## Recent amounts\n");
    for amount in &overview.recent_amounts {
        let _ = writeln!(output, "- {} {}", literal(&amount.currency), literal(&amount.amount));
    }
    output.push_str("\n## Open actions\n");
    for item in &overview.open_action_items {
        let _ = writeln!(output, "- {}: {}", literal(&item.subject), literal(&item.action));
    }
    output.push_str("\n## Top junk domains\n");
    for domain in &overview.unsubscribe_candidates {
        let _ = writeln!(output, "- {}: {} messages", literal(&domain.key), domain.message_count);
    }
    output
}
