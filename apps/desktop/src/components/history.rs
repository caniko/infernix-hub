//! The donor `AuditFeed` (newest-first history rows) adapted for hub history:
//! `ContentDetail.history`, `RelationshipDetail.history`, and the Today feed.

use dioxus::prelude::*;

/// A presentation row; callers format the relative time.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryLine {
    pub event: String,
    pub detail: String,
    pub when: String,
    pub seq: String,
}

#[component]
pub fn HistoryFeed(entries: Vec<HistoryLine>) -> Element {
    rsx! {
        div { class: "audit-feed hairline-list",
            if entries.is_empty() {
                p { class: "empty", "No history yet." }
            } else {
                for e in entries.iter() {
                    div { class: "history-row hairline-row", key: "{e.seq}-{e.event}",
                        span { class: "ev", "{e.event}" }
                        span { class: "when", "{e.when}" }
                        if !e.detail.is_empty() {
                            span { class: "detail", "{e.detail}" }
                        }
                        span { class: "seq mono", "#{e.seq}" }
                    }
                }
            }
        }
    }
}
