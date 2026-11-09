//! Static workspace placeholders for the shell milestone. Milestone C replaces
//! the demo data with `HubClient` responses; the layout and components stay.

use dioxus::prelude::*;

use crate::components::chat::{ChatPanel, TranscriptLine};
use crate::components::history::{HistoryFeed, HistoryLine};
use crate::components::proposal::{DeltaRow, ListEditor, ProposalCard};
use crate::components::{Chip, EmptyState, GatePrompt, Notice, Panel, StatCard};
use crate::shell::ContextItem;

fn demo_history() -> Vec<HistoryLine> {
    vec![
        HistoryLine {
            event: "release.frozen".into(),
            detail: "rev 3 · 2 accounts · hub-release-v1:9f2c…".into(),
            when: "4m ago".into(),
            seq: "128".into(),
        },
        HistoryLine {
            event: "asset.selected".into(),
            detail: "asset 7c1e… · alt text set".into(),
            when: "12m ago".into(),
            seq: "127".into(),
        },
        HistoryLine {
            event: "canon.proposed".into(),
            detail: "persona northwind · revision 4".into(),
            when: "31m ago".into(),
            seq: "126".into(),
        },
        HistoryLine {
            event: "generation.dispatched".into(),
            detail: "windmill root 01J… · 4 variants".into(),
            when: "48m ago".into(),
            seq: "125".into(),
        },
    ]
}

#[component]
pub fn TodayView() -> Element {
    let buckets = [
        ("Pending reviews", "2"),
        ("Active production", "1"),
        ("Due releases", "3"),
        ("Failed operations", "0"),
        ("Overdue interactions", "1"),
    ];
    rsx! {
        aside { class: "context",
            div { class: "context-head",
                h2 { "Today" }
                span { class: "label-tech", "static" }
            }
            div { class: "context-list",
                for (name, count) in buckets {
                    div { class: "context-item", key: "{name}",
                        span { class: "ctx-title", "{name}" }
                        span { class: "ctx-meta", "{count}" }
                    }
                }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "Today" }
                    p { class: "hint", "Reviews waiting, production running, releases due, and relationships going cold." }
                }
            }
            div { class: "stat-row",
                StatCard { label: "Pending reviews", value: "2", tone: Some("warn".into()), detail: Some("awaiting operator".into()) }
                StatCard { label: "Active production", value: "1", tone: None, detail: Some("windmill + comfy".into()) }
                StatCard { label: "Due releases", value: "3", tone: None, detail: Some("next 24h".into()) }
                StatCard { label: "Failed operations", value: "0", tone: Some("ok".into()), detail: Some("no action needed".into()) }
            }
            Panel { title: "History", caption: Some("newest 4".into()),
                HistoryFeed { entries: demo_history() }
            }
        }
    }
}

#[component]
pub fn NewsView() -> Element {
    let mut notice = use_signal(|| Option::<String>::None);
    let headlines = [
        ("EU approves the AI Act implementation timeline", "wire · 6 sources · coverage 12"),
        ("Open-weight model release shifts inference pricing", "feed · 3 sources · coverage 5"),
        ("Grid operator publishes 2027 capacity auction", "wire · 2 sources · coverage 3"),
    ];
    rsx! {
        aside { class: "context",
            div { class: "context-head", h2 { "News" } span { class: "label-tech", "3 items" } }
            div { class: "context-list",
                for (i, (title, meta)) in headlines.iter().enumerate() {
                    ContextItem {
                        key: "{title}",
                        title: title.to_string(),
                        meta: meta.to_string(),
                        active: i == 0,
                        on_select: move |_| {},
                    }
                }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "EU approves the AI Act implementation timeline" }
                    p { class: "hint", "canonical URL · 6 sources · first seen 2h ago · coverage 12" }
                }
            }
            Panel { title: "Evidence", caption: Some("quoted sources".into()),
                blockquote { class: "quote",
                    "Member states must designate market-surveillance authorities before the first compliance window."
                    cite { "— wire source, 09:14 UTC" }
                }
                blockquote { class: "quote",
                    "The phased timetable keeps general-purpose model obligations in the first wave."
                    cite { "— second source, 09:31 UTC" }
                }
            }
            Panel { title: "Create brief", caption: Some("static preview".into()),
                div { class: "form-grid",
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "news-persona", "Persona" }
                        select { id: "news-persona", class: "input",
                            option { "northwind" }
                            option { "harbor" }
                        }
                    }
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "news-angle", "Angle" }
                        input { id: "news-angle", class: "input", placeholder: "what the operator wants to say" }
                    }
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "news-audience", "Audience" }
                        input { id: "news-audience", class: "input", placeholder: "who it is for" }
                    }
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "news-copy", "Copy direction" }
                        textarea { id: "news-copy", class: "input", placeholder: "tone, structure, claims to avoid" }
                    }
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "news-visual", "Visual direction" }
                        textarea { id: "news-visual", class: "input", placeholder: "composition, palette, references" }
                    }
                    div { class: "actions",
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| notice.set(Some("Static preview — the brief POST lands in milestone C.".into())),
                            "Create brief"
                        }
                    }
                    if let Some(n) = notice() {
                        Notice { ok: true, message: n }
                    }
                }
            }
        }
    }
}

#[component]
pub fn StudioView() -> Element {
    let mut four_up = use_signal(|| false);
    let mut notice = use_signal(|| Option::<String>::None);
    let items = [
        ("AI Act timeline explainer", "producing · rev 3"),
        ("Inference pricing teardown", "review · rev 2"),
        ("Grid auction thread", "brief · rev 1"),
    ];
    rsx! {
        aside { class: "context",
            div { class: "context-head", h2 { "Content" } span { class: "label-tech", "3 items" } }
            div { class: "context-list",
                for (i, (title, meta)) in items.iter().enumerate() {
                    ContextItem {
                        key: "{title}",
                        title: title.to_string(),
                        meta: meta.to_string(),
                        active: i == 0,
                        on_select: move |_| {},
                    }
                }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "AI Act timeline explainer" }
                    p { class: "hint", "producing · 4 variants · 2 accounts · revision 3" }
                }
                div { class: "spacer" }
                Chip { label: "Generation running", tone: Some("amber".into()) }
            }
            Panel { title: "Brief", caption: Some("frozen at creation".into()),
                div { class: "kv-grid",
                    div { class: "kv", span { class: "k", "Angle" }, span { class: "v", "Compliance dates, not politics" } }
                    div { class: "kv", span { class: "k", "Audience" }, span { class: "v", "Product and policy leads" } }
                    div { class: "kv", span { class: "k", "Copy" }, span { class: "v", "Plain, dated, no speculation" } }
                    div { class: "kv", span { class: "k", "Visual" }, span { class: "v", "Timeline, warm neutrals" } }
                }
            }
            Panel { title: "Generation", caption: Some("1 attempt running".into()),
                div { class: "glass-card generation",
                    div { class: "spread",
                        span { class: "label-tech", "windmill_root · 01J8Q…" }
                        Chip { label: "Running", tone: Some("amber".into()) }
                    }
                    div { class: "progress", div { class: "fill", style: "width: 66%;" } }
                    div { class: "attempts",
                        div { class: "attempt",
                            span { class: "external", "comfy_prompt · 9f2c…" }
                            span { "queued" }
                        }
                    }
                    div { class: "actions",
                        button {
                            class: "btn btn-glass danger",
                            onclick: move |_| notice.set(Some("Static preview — cancel lands in milestone C.".into())),
                            "Cancel"
                        }
                    }
                }
            }
            Panel { title: "Assets", caption: Some("select one".into()),
                div { class: "asset-toolbar",
                    button {
                        class: if four_up() { "btn btn-glass" } else { "btn btn-primary" },
                        onclick: move |_| four_up.set(false),
                        "2-up"
                    }
                    button {
                        class: if four_up() { "btn btn-primary" } else { "btn btn-glass" },
                        onclick: move |_| four_up.set(true),
                        "4-up"
                    }
                }
                div { class: if four_up() { "asset-grid four" } else { "asset-grid two" },
                    for i in 0..if four_up() { 4 } else { 2 } {
                        div { class: "glass-card asset-tile", key: "{i}",
                            div { class: "asset-frame",
                                span { class: "asset-missing", "asset {i} · image loads from /v1/assets in milestone C" }
                            }
                            div { class: "asset-meta",
                                span { class: "label-tech", "asset-{i}" }
                                span { class: "chip", "ready" }
                            }
                            button { class: "btn btn-glass btn-xs", "Select" }
                        }
                    }
                }
            }
            Panel { title: "Freeze release", caption: Some("exact action".into()),
                div { class: "form-grid",
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "release-text", "Text" }
                        textarea { id: "release-text", class: "input", placeholder: "the exact post text" }
                    }
                    div { class: "field-row",
                        label { class: "label-tech", "Accounts" },
                        div { class: "stack",
                            label { class: "row", input { r#type: "checkbox", checked: true }, "mastodon · @northwind" }
                            label { class: "row", input { r#type: "checkbox" }, "x · @northwind" }
                        }
                    }
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "release-before", "Not before" }
                        input { id: "release-before", class: "input", r#type: "datetime-local" }
                    }
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "release-after", "Not after" }
                        input { id: "release-after", class: "input", r#type: "datetime-local" }
                    }
                    div { class: "actions",
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| notice.set(Some("Static preview — freeze release lands in milestone C.".into())),
                            "Freeze release"
                        }
                    }
                    if let Some(n) = notice() {
                        Notice { ok: true, message: n }
                    }
                }
            }
        }
    }
}

#[component]
pub fn CalendarView() -> Element {
    rsx! {
        aside { class: "context",
            div { class: "context-head", h2 { "Intents" } span { class: "label-tech", "static" } }
            div { class: "context-list",
                div { class: "context-item active", span { class: "ctx-title", "Today" }, span { class: "ctx-meta", "3 intents" } }
                div { class: "context-item", span { class: "ctx-title", "Tomorrow" }, span { class: "ctx-meta", "1 intent" } }
            }
        }
        main { class: "work",
            div { class: "work-head", div { h1 { "Calendar" } p { class: "hint", "Grouped by day and account; approval, delivery and provider state per intent." } } }
            section { class: "calendar-day",
                h2 { class: "label-tech", "Today · 2026-11-09" }
                div { class: "glass-card calendar-account",
                    div { class: "spread",
                        span { "mastodon · @northwind" }
                        Chip { label: "Approved", tone: Some("ok".into()) }
                    }
                    div { class: "intent-row",
                        span { class: "time", "09:30" }
                        span { "AI Act timeline explainer" }
                        Chip { label: "Scheduled", tone: Some("info".into()) }
                        Chip { label: "Delivery pending", tone: Some("muted".into()) }
                    }
                    div { class: "intent-row",
                        span { class: "time", "13:00" }
                        span { "Inference pricing teardown" }
                        Chip { label: "Published", tone: Some("ok".into()) }
                        span { class: "label-tech", "provider 1883…" }
                    }
                }
                div { class: "glass-card calendar-account",
                    div { class: "spread",
                        span { "x · @northwind" }
                        Chip { label: "In review", tone: Some("amber".into()) }
                    }
                    div { class: "intent-row",
                        span { class: "time", "17:15" }
                        span { "AI Act timeline explainer" }
                        Chip { label: "Scheduled", tone: Some("info".into()) }
                    }
                }
            }
            section { class: "calendar-day",
                h2 { class: "label-tech", "Tomorrow · 2026-11-10" }
                div { class: "glass-card calendar-account",
                    div { class: "spread", span { "mastodon · @northwind" }, Chip { label: "Draft", tone: Some("muted".into()) } }
                    div { class: "intent-row", span { class: "time", "08:00" }, span { "Grid auction thread" }, Chip { label: "Scheduled", tone: Some("info".into()) } }
                }
            }
        }
    }
}

#[component]
pub fn RelationshipsView() -> Element {
    let mut notice = use_signal(|| Option::<String>::None);
    let mut lines = use_signal(|| {
        vec![
            TranscriptLine { role: "system".into(), text: "Thread · mastodon · @gridwatch · owner agent · epoch 4".into(), proposal: None },
            TranscriptLine { role: "operator".into(), text: "Thanks — the auction date is now in the post.".into(), proposal: None },
            TranscriptLine { role: "assistant".into(), text: "Drafted a reply; it is waiting for your approval in Drafts.".into(), proposal: Some("Review canon proposal".into()) },
        ]
    });
    let rels = [
        ("@gridwatch · mastodon", "agent · epoch 4 · follow-up overdue"),
        ("@policywire · x", "human · you · epoch 2"),
    ];
    rsx! {
        aside { class: "context",
            div { class: "context-head", h2 { "Relationships" } span { class: "label-tech", "2 threads" } }
            div { class: "context-list",
                for (i, (title, meta)) in rels.iter().enumerate() {
                    ContextItem {
                        key: "{title}",
                        title: title.to_string(),
                        meta: meta.to_string(),
                        active: i == 0,
                        on_select: move |_| {},
                    }
                }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "@gridwatch" }
                    p { class: "hint", "mastodon · owner agent · owner epoch 4 · follow-up overdue" }
                }
                div { class: "spacer" }
                Chip { label: "Agent owned", tone: Some("info".into()) }
            }
            Panel { title: "Thread", caption: Some("inbound and outbound".into()),
                div { class: "thread",
                    div { class: "interaction inbound",
                        div { class: "meta", span { class: "label-tech", "inbound" }, span { class: "label-tech", "2h ago" } }
                        div { class: "body", "Is the auction date confirmed, or still provisional?" }
                    }
                    div { class: "interaction outbound",
                        div { class: "meta", span { class: "label-tech", "outbound" }, span { class: "label-tech", "1h ago" } }
                        div { class: "body", "Confirmed — 14 March, per the operator's notice." }
                    }
                }
            }
            Panel { title: "Drafts", caption: Some("approval required".into()),
                div { class: "draft",
                    p { "“Happy to send the filing reference if that helps.”" }
                    GatePrompt {
                        label: "assistant draft · epoch 4",
                        approve_label: "Approve",
                        reject_label: "Reject",
                        on_approve: move |_| notice.set(Some("Static preview — draft approval lands in milestone C.".into())),
                        on_reject: move |_| notice.set(Some("Static preview — draft rejection lands in milestone C.".into())),
                    }
                }
                div { class: "btn-row",
                    button { class: "btn btn-glass", onclick: move |_| notice.set(Some("Take over lands in milestone C.".into())), "Take over" }
                    button { class: "btn btn-glass", onclick: move |_| notice.set(Some("Release to agent lands in milestone C.".into())), "Release to agent" }
                    button { class: "btn btn-glass", onclick: move |_| notice.set(Some("Handoff lands in milestone C.".into())), "Open in platform" }
                }
            }
            Panel { title: "Creative chat", caption: Some("content 7c1e…".into()),
                ChatPanel {
                    lines: lines(),
                    disabled: false,
                    placeholder: "message…".to_string(),
                    on_send: move |text: String| {
                        lines.write().push(TranscriptLine { role: "operator".into(), text, proposal: None });
                        lines.write().push(TranscriptLine { role: "assistant".into(), text: "Static preview reply.".into(), proposal: None });
                    },
                    on_review: move |_: usize| notice.set(Some("Proposal review opens in Library (milestone C).".into())),
                }
                if let Some(n) = notice() {
                    Notice { ok: true, message: n }
                }
            }
        }
    }
}

#[component]
pub fn LibraryView() -> Element {
    let rules = use_signal(|| vec!["No hype superlatives".to_string(), "Dates over adjectives".to_string()]);
    let mut notice = use_signal(|| Option::<String>::None);
    rsx! {
        aside { class: "context",
            div { class: "context-head", h2 { "Library" } span { class: "label-tech", "1 persona" } }
            div { class: "context-list",
                ContextItem { title: "northwind".to_string(), meta: "canon rev 3 · 1 pending".to_string(), active: true, on_select: move |_| {} }
                ContextItem { title: "harbor".to_string(), meta: "canon rev 1 · 0 pending".to_string(), active: false, on_select: move |_| {} }
            }
        }
        main { class: "work",
            div { class: "work-head",
                div { h1 { "northwind" } p { class: "hint", "voice, rules, avoid list and world canon. Only the operator accepts changes." } }
                div { class: "spacer" }
                Chip { label: "Accepted rev 3", tone: Some("ok".into()) }
            }
            Panel { title: "Pending proposals", caption: Some("review required".into()),
                div { class: "proposal-list",
                    ProposalCard {
                        title: "voice".to_string(),
                        description: "Dry, precise, no exclamation marks.".to_string(),
                        reason: Some("Learned from three operator edits".to_string()),
                        editable: Some("Dry, precise; no exclamation marks.".to_string()),
                        focused: false,
                        on_accept: move |edited: Option<String>| {
                            notice.set(Some(match edited {
                                Some(v) => format!("Accepted with edit: {v}"),
                                None => "Accepted proposal (static preview).".to_string(),
                            }));
                        },
                        on_reject: move |reason: Option<String>| {
                            notice.set(Some(match reason {
                                Some(r) => format!("Rejected: {r}"),
                                None => "Rejected proposal (static preview).".to_string(),
                            }));
                        },
                    }
                }
                if let Some(n) = notice() {
                    Notice { ok: true, message: n }
                }
            }
            Panel { title: "Canon history", caption: Some("newest first".into()),
                div { class: "delta-list",
                    DeltaRow { seq: "3".to_string(), category: "accepted".to_string(), description: "voice + 2 rules · operator".to_string(), action: None, on_confirm: move |_| {} }
                    DeltaRow { seq: "2".to_string(), category: "superseded".to_string(), description: "world · 1 entry · agent".to_string(), action: None, on_confirm: move |_| {} }
                }
            }
            Panel { title: "Rules", caption: Some("submit as proposal".into()),
                ListEditor {
                    title: "Rules".to_string(),
                    hint: "Hard constraints the copy must follow. Saving submits a proposal — it never writes canon directly.".to_string(),
                    items: rules,
                    placeholder: "Add a rule…".to_string(),
                }
                div { class: "actions",
                    button {
                        class: "btn btn-primary",
                        onclick: move |_| notice.set(Some("Static preview — canon proposal POST lands in milestone C.".into())),
                        "Submit proposal"
                    }
                }
            }
            Panel { title: "Assets", caption: Some("all ready assets".into()),
                EmptyState { title: "Asset library is empty in the shell preview.".to_string(), hint: Some("Milestone C lists /v1/assets with thumbnails.".to_string()) }
            }
        }
    }
}
