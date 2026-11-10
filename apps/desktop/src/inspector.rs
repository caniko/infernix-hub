//! Right-hand inspector: technical ids, foreign ids (paperclip issue/approval,
//! prompt ids, provider ids), release payload hash, raw JSON and history.
//! Built from contract types so every workspace can feed the same panel.

use contracts::{CanonRevision, ContentDetail, NewsItem, Persona, RelationshipDetail};
use dioxus::prelude::*;
use serde::Serialize;

use crate::{
    components::history::{HistoryFeed, HistoryLine},
    format::{fmt_dt, rel_time, short_hash},
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InspectorData {
    pub title: String,
    pub ids: Vec<(String, String)>,
    pub foreign: Vec<(String, String)>,
    pub payload_hash: Option<String>,
    pub raw: String,
    pub history: Vec<HistoryLine>,
}

fn pretty<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
}

fn history_line(entry: &contracts::HistoryEntry) -> HistoryLine {
    HistoryLine {
        event: entry.action.clone(),
        detail: format!("{} · {}", entry.subject_type, short_hash(&entry.subject_id)),
        when: rel_time(entry.at.timestamp_millis().max(0) as u64),
        seq: entry.id.to_string(),
    }
}

fn history(entries: &[contracts::HistoryEntry]) -> Vec<HistoryLine> {
    let mut ordered: Vec<&contracts::HistoryEntry> = entries.iter().collect();
    ordered.sort_by_key(|entry| std::cmp::Reverse(entry.at));
    ordered.into_iter().map(history_line).collect()
}

fn optional(label: &str, value: &Option<String>) -> Option<(String, String)> {
    value.as_ref().filter(|v| !v.is_empty()).map(|v| (label.to_string(), v.clone()))
}

pub fn from_content(detail: &ContentDetail) -> InspectorData {
    let item = &detail.item;
    let latest_release = detail.releases.first(); // newest first
    let mut foreign = Vec::new();
    if let Some((key, value)) = optional("paperclip issue", &item.paperclip_issue_id) {
        foreign.push((key, value));
    }
    if let Some((key, value)) = optional("paperclip issue url", &item.paperclip_issue_url) {
        foreign.push((key, value));
    }
    if let Some(release) = latest_release
        && let Some((key, value)) = optional("paperclip approval", &release.paperclip_approval_id)
    {
        foreign.push((key, value));
    }
    for generation in &detail.generations {
        foreign.push((
            format!("workflow {}", short_hash(&generation.workflow_id.to_string())),
            format!("request v{} · {} variants · {} attempts", generation.request_version, generation.variants, generation.attempts.len()),
        ));
    }
    for asset in &detail.assets {
        foreign.push((
            format!("asset {} provenance", short_hash(&asset.id.to_string())),
            serde_json::to_string(&asset.provenance).unwrap_or_else(|_| "{}".into()),
        ));
    }
    for intent in &detail.intents {
        if let Some((key, value)) = optional("provider post", &intent.provider_post_id) {
            foreign.push((format!("{key} · {}", short_hash(&intent.account_id)), value));
        }
        if let Some((key, value)) = optional("provider url", &intent.provider_url) {
            foreign.push((format!("{key} · {}", short_hash(&intent.account_id)), value));
        }
    }

    InspectorData {
        title: format!("Content · {}", short_hash(&item.id.to_string())),
        ids: vec![
            ("content id".into(), item.id.to_string()),
            ("persona id".into(), item.persona_id.to_string()),
            ("news item id".into(), item.news_item_id.map(|id| id.to_string()).unwrap_or_else(|| "—".into())),
            ("state".into(), item.state.to_string()),
            ("created".into(), fmt_dt(item.created_at)),
            ("updated".into(), fmt_dt(item.updated_at)),
        ],
        foreign,
        payload_hash: latest_release.map(|release| release.payload_hash.clone()),
        raw: pretty(detail),
        history: history(&detail.history),
    }
}

pub fn from_relationship(detail: &RelationshipDetail) -> InspectorData {
    let relationship = &detail.relationship;
    let mut foreign = Vec::new();
    if let Some((key, value)) = optional("permalink", &relationship.permalink) {
        foreign.push((key, value));
    }
    if let Some(id) = relationship.content_item_id {
        foreign.push(("content item".into(), id.to_string()));
    }
    for draft in &detail.drafts {
        if let Some((key, value)) = optional("provider reply", &draft.provider_reply_id) {
            foreign.push((format!("{key} · draft {}", short_hash(&draft.id.to_string())), value));
        }
    }

    InspectorData {
        title: format!("Relationship · {}", short_hash(&relationship.id.to_string())),
        ids: vec![
            ("relationship id".into(), relationship.id.to_string()),
            ("account".into(), relationship.account_id.clone()),
            ("thread".into(), relationship.external_thread_id.clone()),
            ("counterpart".into(), relationship.counterpart.clone()),
            ("owner".into(), relationship.owner.to_string()),
            ("owner user".into(), relationship.owner_user.clone().unwrap_or_else(|| "—".into())),
            ("owner epoch".into(), relationship.owner_epoch.to_string()),
            ("follow up".into(), relationship.follow_up_at.map(fmt_dt).unwrap_or_else(|| "—".into())),
        ],
        foreign,
        payload_hash: None,
        raw: pretty(detail),
        history: history(&detail.history),
    }
}

pub fn from_persona(persona: &Persona, revisions: &[CanonRevision]) -> InspectorData {
    let accepted = revisions.iter().find(|revision| revision.state == contracts::CanonState::Accepted);
    let mut ordered: Vec<&CanonRevision> = revisions.iter().collect();
    ordered.sort_by_key(|revision| std::cmp::Reverse(revision.revision));
    let history: Vec<HistoryLine> = ordered
        .into_iter()
        .map(|revision| HistoryLine {
            event: format!("revision {} · {}", revision.revision, revision.state),
            detail: format!("{} · {}", revision.source, revision.proposed_by),
            when: rel_time(revision.created_at.timestamp_millis().max(0) as u64),
            seq: revision.revision.to_string(),
        })
        .collect();

    InspectorData {
        title: format!("Persona · {}", persona.name),
        ids: vec![
            ("persona id".into(), persona.id.to_string()),
            ("company".into(), persona.company_id.clone()),
            ("accepted revision".into(), persona.canon.as_ref().map(|canon| canon.revision.to_string()).unwrap_or_else(|| "—".into())),
            ("pending proposals".into(), persona.pending_proposals.to_string()),
        ],
        foreign: accepted.map(|revision| vec![("canon id".into(), revision.id.to_string())]).unwrap_or_default(),
        payload_hash: None,
        raw: pretty(&(persona, revisions)),
        history,
    }
}

pub fn from_news(item: &NewsItem) -> InspectorData {
    InspectorData {
        title: format!("News · {}", short_hash(&item.id.to_string())),
        ids: vec![
            ("news id".into(), item.id.to_string()),
            ("revision".into(), item.revision.to_string()),
            ("coverage".into(), item.coverage.to_string()),
            ("sources".into(), item.sources.join(", ")),
            ("first seen".into(), fmt_dt(item.first_seen_at)),
            ("content item".into(), item.content_item_id.map(|id| id.to_string()).unwrap_or_else(|| "—".into())),
        ],
        foreign: vec![("canonical url".into(), item.canonical_url.clone())],
        payload_hash: None,
        raw: pretty(item),
        history: Vec::new(),
    }
}

#[component]
pub fn Inspector(data: Option<InspectorData>, on_close: EventHandler<()>) -> Element {
    rsx! {
        aside { class: "inspector", aria_label: "Inspector",
            div { class: "inspector-head",
                span { class: "label-tech", "Inspector" }
                button { class: "icon-btn", onclick: move |_| on_close.call(()), aria_label: "Close inspector", "✕" }
            }
            match data {
                None => rsx! {
                    p { class: "empty label-tech", "Select an item to inspect its ids, provenance and history." }
                },
                Some(d) => rsx! {
                    h4 { "{d.title}" }
                    section { class: "panel",
                        span { class: "label-tech", "Ids" }
                        div { class: "kv-grid",
                            for (k, v) in d.ids.iter() {
                                div { class: "kv", key: "{k}",
                                    span { class: "k", "{k}" }
                                    span { class: "v", "{v}" }
                                }
                            }
                        }
                    }
                    if !d.foreign.is_empty() {
                        section { class: "panel",
                            span { class: "label-tech", "Foreign ids" }
                            div { class: "kv-grid",
                                for (k, v) in d.foreign.iter() {
                                    div { class: "kv", key: "{k}",
                                        span { class: "k", "{k}" }
                                        span { class: "v", "{v}" }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(hash) = d.payload_hash {
                        section { class: "panel",
                            span { class: "label-tech", "Payload hash" }
                            pre { class: "raw-json", "{hash}" }
                        }
                    }
                    section { class: "panel",
                        span { class: "label-tech", "Raw JSON" }
                        pre { class: "raw-json", "{d.raw}" }
                    }
                    section { class: "panel",
                        span { class: "label-tech", "History" }
                        HistoryFeed { entries: d.history }
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use contracts::{CanonBody, CanonSource, CanonState};

    #[test]
    fn persona_inspector_exposes_canon_ids_and_history() {
        let persona = Persona {
            id: uuid::Uuid::new_v4(),
            company_id: "northwind".into(),
            name: "Field Notes".into(),
            canon: None,
            pending_proposals: 1,
        };
        let revision = CanonRevision {
            id: uuid::Uuid::new_v4(),
            persona_id: persona.id,
            revision: 3,
            base_revision: Some(2),
            body: CanonBody::default(),
            state: CanonState::Accepted,
            source: CanonSource::Operator,
            rationale: None,
            proposed_by: "operator".into(),
            decided_by: Some("operator".into()),
            created_at: Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(),
        };
        let data = from_persona(&persona, std::slice::from_ref(&revision));
        assert!(data.ids.iter().any(|(k, _)| k == "persona id"));
        assert!(data.foreign.iter().any(|(_, v)| *v == revision.id.to_string()));
        assert_eq!(data.history.len(), 1);
        assert_eq!(data.history[0].seq, "3");
    }
}
