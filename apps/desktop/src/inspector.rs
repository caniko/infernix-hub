//! Selection summary, audit history, and opt-in diagnostics.
//! Built from contract types so every workspace can feed the same panel.

use contracts::{CanonRevision, ContentDetail, NewsItem, Persona, RelationshipDetail};
use dioxus::prelude::*;
use serde::Serialize;

use crate::{
    components::{
        Chip,
        history::{HistoryFeed, HistoryLine},
    },
    format::{fmt_dt, rel_time, short_hash},
    labels,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InspectorData {
    pub title: String,
    pub status: (&'static str, &'static str),
    pub summary: Vec<(&'static str, String)>,
    pub failures: Vec<String>,
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
        event: entry.action.replace(['_', '.'], " "),
        detail: entry.detail.as_str().unwrap_or_else(|| labels::author(&entry.actor)).to_string(),
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
        title: item.title.clone(),
        status: labels::content_state(item.state),
        summary: vec![
            ("Audience", item.brief.audience.clone()),
            ("Created", fmt_dt(item.created_at)),
            ("Updated", fmt_dt(item.updated_at)),
        ],
        ids: vec![
            ("content id".into(), item.id.to_string()),
            ("persona id".into(), item.persona_id.to_string()),
            ("news item id".into(), item.news_item_id.map(|id| id.to_string()).unwrap_or_else(|| "—".into())),
        ],
        foreign,
        payload_hash: latest_release.map(|release| release.payload_hash.clone()),
        raw: pretty(detail),
        history: history(&detail.history),
        ..Default::default()
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
        title: relationship.counterpart.clone(),
        status: labels::owner(relationship.owner),
        summary: vec![
            ("Assigned to", relationship.owner_user.clone().unwrap_or_else(|| "No human assignee".into())),
            ("Follow-up", relationship.follow_up_at.map(fmt_dt).unwrap_or_else(|| "Not scheduled".into())),
        ],
        ids: vec![
            ("relationship id".into(), relationship.id.to_string()),
            ("account".into(), relationship.account_id.clone()),
            ("thread".into(), relationship.external_thread_id.clone()),
            ("owner epoch".into(), relationship.owner_epoch.to_string()),
        ],
        foreign,
        raw: pretty(detail),
        history: history(&detail.history),
        ..Default::default()
    }
}

pub fn from_persona(persona: &Persona, revisions: &[CanonRevision]) -> InspectorData {
    let accepted = revisions.iter().find(|revision| revision.state == contracts::CanonState::Accepted).or(persona.canon.as_ref());
    let mut ordered: Vec<&CanonRevision> = revisions.iter().collect();
    ordered.sort_by_key(|revision| std::cmp::Reverse(revision.revision));
    let history: Vec<HistoryLine> = ordered
        .into_iter()
        .map(|revision| HistoryLine {
            event: format!("Revision {} · {}", revision.revision, labels::canon_state(revision.state).0),
            detail: revision.rationale.clone().unwrap_or_else(|| revision.proposed_by.clone()),
            when: rel_time(revision.created_at.timestamp_millis().max(0) as u64),
            seq: revision.revision.to_string(),
        })
        .collect();

    InspectorData {
        title: persona.name.clone(),
        status: if accepted.is_some() { ("Canon accepted", "ok") } else { ("No accepted canon", "muted") },
        summary: vec![
            ("Accepted revision", accepted.map(|revision| revision.revision.to_string()).unwrap_or_else(|| "None".into())),
            ("Pending proposals", persona.pending_proposals.to_string()),
        ],
        ids: vec![("persona id".into(), persona.id.to_string()), ("company".into(), persona.company_id.clone())],
        foreign: accepted.map(|revision| vec![("canon id".into(), revision.id.to_string())]).unwrap_or_default(),
        raw: pretty(&(persona, revisions)),
        history,
        ..Default::default()
    }
}

pub fn from_news(item: &NewsItem) -> InspectorData {
    let plural = if item.coverage == 1 { "" } else { "s" };
    let sources: Vec<_> = if item.sources.is_empty() {
        item.evidence.iter().map(|entry| entry.url.as_str()).map(labels::news_source).collect()
    } else {
        item.sources.iter().map(String::as_str).map(labels::news_source).collect()
    };
    InspectorData {
        title: item.title.clone(),
        status: if item.content_item_id.is_some() { ("Brief created", "ok") } else { ("No linked brief", "muted") },
        summary: vec![("Sources", sources.join(", ")), ("Coverage", format!("{} mention{plural}", item.coverage))],
        ids: vec![
            ("news id".into(), item.id.to_string()),
            ("revision".into(), item.revision.to_string()),
            ("content item".into(), item.content_item_id.map(|id| id.to_string()).unwrap_or_else(|| "—".into())),
        ],
        foreign: vec![("canonical url".into(), item.canonical_url.clone())],
        raw: pretty(item),
        ..Default::default()
    }
}

/// Hosts are identified by name in the snapshot, so there is no id read; the
/// inspector carries the full revision, system path and failed checks.
pub fn from_host(host: &contracts::fleet::HostStatus, target: Option<&str>) -> InspectorData {
    let update = host.update_state(target);
    let mut summary = vec![("Current revision", host.revision.as_deref().map_or_else(|| "—".into(), short_hash))];
    let mut ids = vec![
        ("revision".into(), host.revision.clone().unwrap_or_else(|| "—".into())),
        ("system path".into(), host.system.clone().unwrap_or_else(|| "—".into())),
    ];
    if let Some(target) = target.filter(|target| !target.is_empty()) {
        summary.push(("Target revision", short_hash(target)));
        ids.push(("target revision".into(), target.to_owned()));
    }
    summary.push(("Revision", labels::update_state(update).0.into()));
    summary.push((
        "Last checked",
        chrono::DateTime::parse_from_rfc3339(&host.checked_at)
            .map(|date| fmt_dt(date.to_utc()))
            .unwrap_or_else(|_| host.checked_at.clone()),
    ));
    InspectorData {
        title: host.name.clone(),
        status: labels::health(host.health),
        summary,
        failures: host.failures.clone(),
        ids,
        raw: pretty(host),
        ..Default::default()
    }
}

#[component]
pub fn Inspector(data: Option<InspectorData>, has_selection: bool, on_close: EventHandler<()>) -> Element {
    rsx! {
        aside { class: "inspector", aria_label: "Inspector",
            header { class: "inspector-head",
                div { class: "inspector-identity",
                    if let Some(d) = &data {
                        h2 { "{d.title}" }
                        Chip { label: d.status.0.to_owned(), tone: Some(d.status.1.to_owned()) }
                    } else {
                        h2 { "Inspector" }
                    }
                }
                button { class: "icon-btn", onclick: move |_| on_close.call(()), aria_label: "Close inspector", "×" }
            }
            match data {
                None => rsx! {
                    p { class: "empty",
                        if has_selection { "Details aren’t available yet." } else { "Select an item to see its details and history." }
                    }
                },
                Some(d) => rsx! {
                    section { class: "inspector-section inspector-summary",
                        if !d.failures.is_empty() {
                            section { class: "inspector-failures severity-row severity-crit",
                                h3 { "Active failures" }
                                ul {
                                    for (index, failure) in d.failures.iter().enumerate() {
                                        li { key: "{index}", "{failure}" }
                                    }
                                }
                            }
                        }
                        dl { class: "kv-grid",
                            for (label, value) in d.summary.iter().filter(|(_, value)| !value.trim().is_empty()) {
                                div {
                                    class: if matches!(*label, "Created" | "Updated" | "First seen" | "Follow-up" | "Last checked")
                                        && value.ends_with(" UTC") { "kv-item inspector-timestamp" } else { "kv-item" },
                                    key: "{label}",
                                    dt { class: "kv-label", "{label}" }
                                    dd { class: "kv-value", "{value}" }
                                }
                            }
                        }
                    }
                    section { class: "inspector-section",
                        h3 { "Audit history" }
                        HistoryFeed { entries: d.history }
                    }
                    details { class: "inspector-diagnostics",
                        summary { "Diagnostics & raw payload" }
                        dl { class: "hairline-list",
                            for (label, value) in d.ids {
                                DiagnosticRow { key: "{label}", label, value }
                            }
                            if let Some(hash) = d.payload_hash {
                                DiagnosticRow { label: "Payload hash".to_string(), value: hash }
                            }
                        }
                        if !d.foreign.is_empty() {
                            h3 { "Foreign IDs" }
                            dl { class: "hairline-list",
                                for (label, value) in d.foreign {
                                    DiagnosticRow { key: "{label}-{value}", label, value }
                                }
                            }
                        }
                        div { class: "spread",
                            h3 { "Raw JSON" }
                            CopyButton { label: "raw JSON".to_string(), value: d.raw.clone() }
                        }
                        pre { class: "raw-json", "{d.raw}" }
                    }
                },
            }
        }
    }
}

#[component]
fn DiagnosticRow(label: String, value: String) -> Element {
    rsx! {
        div { class: "diagnostic-row hairline-row",
            dt { class: "kv-label", "{label}" }
            dd { class: "kv-value mono",
                span { "{value}" }
                if value.len() > 24 {
                    CopyButton { label, value }
                }
            }
        }
    }
}

#[component]
fn CopyButton(label: String, value: String) -> Element {
    let mut status = use_signal(|| "");
    rsx! {
        button {
            class: "btn secondary btn-xs",
            aria_label: "Copy {label}",
            onclick: move |_| {
                let value = value.clone();
                spawn(async move {
                    let eval = document::eval("const text = await dioxus.recv(); await navigator.clipboard.writeText(text); return true;");
                    let copied = eval.send(value).is_ok() && eval.join::<bool>().await.unwrap_or(false);
                    status.set(if copied { "Copied" } else { "Clipboard unavailable" });
                });
            },
            "Copy"
        }
        if !status().is_empty() {
            small { class: "copy-status", role: "status", "{status}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use contracts::{CanonBody, CanonSource, CanonState};

    #[test]
    fn audit_history_shows_operator_when_there_is_no_text_detail() {
        let mut entry = contracts::HistoryEntry {
            id: 1,
            at: Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(),
            actor: "operator:operator".into(),
            action: "assigned".into(),
            subject_type: "relationship".into(),
            subject_id: "thread".into(),
            correlation_id: None,
            detail: serde_json::Value::Null,
        };
        assert_eq!(history_line(&entry).detail, "Operator");
        entry.detail = "Assigned to Casey".into();
        assert_eq!(history_line(&entry).detail, "Assigned to Casey");
    }

    #[test]
    fn host_failures_are_summary_data_not_foreign_ids() {
        let revision = "a".repeat(40);
        let host = contracts::fleet::HostStatus {
            name: "node-a".into(),
            health: contracts::fleet::Health::Degraded,
            revision: Some(revision.clone()),
            system: None,
            failures: vec!["A service has been down for ten minutes.".into()],
            checked_at: "2026-11-09T12:00:00Z".into(),
        };
        let target = "b".repeat(40);
        let data = from_host(&host, Some(&target));
        assert_eq!(data.status, ("Degraded", "warn"));
        assert!(data.summary.contains(&("Revision", "Other revision".into())));
        assert!(data.summary.contains(&("Current revision", short_hash(&revision))));
        assert!(data.summary.contains(&("Target revision", short_hash(&target))));
        assert!(data.summary.contains(&("Last checked", "2026-11-09 12:00 UTC".into())));
        assert_eq!(data.failures, host.failures);
        assert!(data.foreign.is_empty());
        assert!(data.ids.contains(&("revision".into(), revision)));
        assert!(!from_host(&host, None).summary.iter().any(|(label, _)| *label == "Target revision"));
    }

    #[test]
    fn news_summary_uses_readable_sources_and_keeps_brief_status() {
        let mut item = NewsItem {
            id: uuid::Uuid::nil(),
            canonical_url: "https://example.com/news".into(),
            title: "News".into(),
            summary: String::new(),
            sources: vec!["n8n:rss-studio".into(), "Independent Gazette".into()],
            coverage: 1,
            revision: 1,
            evidence: Vec::new(),
            first_seen_at: Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(),
            content_item_id: None,
        };
        let data = from_news(&item);
        assert_eq!(data.status, ("No linked brief", "muted"));
        assert_eq!(data.summary, [("Sources", "Studio RSS feed, Independent Gazette".into()), ("Coverage", "1 mention".into())]);
        item.content_item_id = Some(uuid::Uuid::nil());
        item.coverage = 2;
        assert_eq!(from_news(&item).status, ("Brief created", "ok"));
        assert!(from_news(&item).summary.contains(&("Coverage", "2 mentions".into())));
        item.sources.clear();
        item.evidence.push(contracts::Evidence { url: "https://example.com/report".into(), quote: "A report.".into() });
        assert_eq!(from_news(&item).summary[0], ("Sources", "https://example.com/report".into()));
    }

    #[test]
    fn persona_inspector_exposes_canon_ids_and_history() {
        let persona = Persona {
            id: uuid::Uuid::new_v4(),
            company_id: "northwind".into(),
            name: "Field Notes".into(),
            canon: None,
            pending_proposals: 1,
            reference_asset_id: None,
            profile: Default::default(),
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
            review_requested_at: None,
            review_issue_url: None,
            comments: Vec::new(),
        };
        let data = from_persona(&persona, std::slice::from_ref(&revision));
        assert!(data.ids.iter().any(|(k, _)| k == "persona id"));
        assert!(data.foreign.iter().any(|(_, v)| *v == revision.id.to_string()));
        assert_eq!(data.history.len(), 1);
        assert_eq!(data.history[0].seq, "3");
    }
}
