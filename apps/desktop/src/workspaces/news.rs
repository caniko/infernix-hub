//! News: coverage + evidence from `/v1/news`, and brief creation into Studio.

use contracts::{Brief, CreateBriefIn, NewsItem};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{EmptyState, ErrorCard, Loading, Panel},
    format,
    labels::news_source,
    shell::ContextItem,
    state::{Store, Workspace},
};

fn coverage_label(mentions: i64, sources: usize) -> String {
    let mention_suffix = if mentions == 1 { "" } else { "s" };
    let source_suffix = if sources == 1 { "" } else { "s" };
    format!("{mentions} mention{mention_suffix} · {sources} source{source_suffix}")
}

#[component]
pub fn NewsView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<contracts::client::HubClient>();
    let news = (store.news).cloned();
    let selected_id = (store.selected_news).cloned();
    let selected: Option<NewsItem> = news.data.as_ref().and_then(|items| items.iter().find(|item| Some(item.id) == selected_id).cloned());

    let mut persona = use_signal(|| Option::<Uuid>::None);
    let mut title = use_signal(String::new);
    let mut angle = use_signal(String::new);
    let mut audience = use_signal(String::new);
    let mut copy = use_signal(String::new);
    let mut visual = use_signal(String::new);
    let mut form_for = use_signal(|| Option::<Uuid>::None);
    let mut pending = use_signal(|| false);
    let personas = (store.personas).cloned();

    use_effect(move || {
        if let Some(id) = (store.selected_news).cloned()
            && let Some(item) = (store.news).cloned().data.and_then(|items| items.into_iter().find(|n| n.id == id))
            && form_for.cloned() != Some(id)
        {
            title.set(item.title.clone());
            angle.set(String::new());
            audience.set(String::new());
            copy.set(String::new());
            visual.set(String::new());
            form_for.set(Some(id));
        }
        if let Some(items) = (store.personas).cloned().data {
            let selected = persona();
            if !items.iter().any(|item| Some(item.id) == selected) {
                let first = items.first().map(|item| item.id);
                if first != selected {
                    persona.set(first);
                }
            }
        }
    });

    let submit = move |event: FormEvent| {
        event.prevent_default();
        if pending() {
            return;
        }
        let Some(persona_id) = persona.cloned() else {
            return;
        };
        let Some(news_item_id) = (store.selected_news).cloned().filter(|id| form_for() == Some(*id)) else {
            return;
        };
        let title_value = title.cloned().trim().to_string();
        if title_value.is_empty() {
            return;
        }
        let body = CreateBriefIn {
            persona_id,
            news_item_id: Some(news_item_id),
            title: title_value,
            brief: Brief {
                angle: angle.cloned(),
                audience: audience.cloned(),
                copy_direction: copy.cloned(),
                visual_direction: visual.cloned(),
                producer: Default::default(),
            },
        };
        let client = client.clone();
        pending.set(true);
        spawn(async move {
            match api::timed(client.create_brief(&body)).await {
                Ok(item) => {
                    store.notify(true, format!("Brief created: {}", item.title));
                    api::refresh_base(store, client).await;
                    pending.set(false);
                    store.set_selected_content(Some(item.id));
                    store.set_workspace(Workspace::Studio);
                }
                Err(error) => {
                    pending.set(false);
                    store.notify(false, format!("{} — {}", error.code, error.message));
                }
            }
        });
    };

    rsx! {
        aside { class: "context", aria_label: "News items",
            div { class: "context-head",
                h2 { "News" }
                span { class: "label-tech", "{news.data.as_ref().map(|v| v.len()).unwrap_or(0)} items" }
            }
            div { class: "context-list",
                match news.data.clone() {
                    Some(items) if items.is_empty() => rsx! { p { class: "ctx-empty", "No news yet." } },
                    Some(items) => rsx! {
                        for item in items {
                            ContextItem {
                                key: "{item.id}",
                                title: item.title.clone(),
                                meta: coverage_label(item.coverage, item.sources.len()),
                                active: Some(item.id) == selected_id,
                                on_select: move |_| store.set_selected_news(Some(item.id)),
                            }
                        }
                    },
                    None if news.error.is_none() => rsx! { p { class: "ctx-empty", "Loading news…" } },
                    None => rsx! {},
                }
            }
        }
        main { class: "work",
            if let Some(error) = news.error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            match selected {
                None if news.loading => rsx! { Loading { text: "Loading news…".to_string() } },
                None if news.data.as_ref().is_some_and(Vec::is_empty) => rsx! {
                    EmptyState { title: "No news yet".to_string(), hint: None }
                },
                None => rsx! { EmptyState { title: "Select a story".to_string(), hint: None } },
                Some(item) => rsx! {
                    header { class: "work-head", style: "flex-wrap: nowrap; align-items: start;",
                        div { style: "min-width: 0; flex: 1; overflow-wrap: anywhere;",
                            h1 { "{item.title}" }
                            p { class: "hint", "{coverage_label(item.coverage, item.sources.len())} · First seen {format::fmt_dt(item.first_seen_at)}" }
                        }
                        button {
                            class: "btn secondary",
                            style: "flex: none;",
                            onclick: {
                                let url = item.canonical_url.clone();
                                move |_| api::open_external(&url)
                            },
                            "Open source"
                        }
                    }
                    p { style: "margin-bottom: 12px; overflow-wrap: anywhere;", "{item.summary}" }
                    if let Some(content) = item.content_item_id {
                        div { class: "toolbar",
                            button {
                                class: "btn secondary btn-xs",
                                onclick: move |_| crate::actions::open_subject(store, "content", content),
                                "Open in Studio"
                            }
                        }
                    }
                    section { class: "panel",
                        h2 { "Create brief" }
                        if let Some(error) = personas.error.clone() {
                            ErrorCard { code: error.code, message: error.message }
                        } else if personas.loading {
                            Loading { text: "Loading personas…".to_string() }
                        }
                        form { class: "form-stack", onsubmit: submit,
                             div { class: "form-field",
                                 label { class: "label-tech", r#for: "news-title",
                                     "Title "
                                     span { aria_hidden: "true", "*" }
                                 }
                                 input {
                                     id: "news-title", class: "input", value: "{title}", required: true,
                                     oninput: move |e| title.set(e.value()),
                                 }
                             }
                             div { class: "form-field",
                                 label { class: "label-tech", r#for: "news-persona",
                                     "Persona "
                                     span { aria_hidden: "true", "*" }
                                 }
                                 select {
                                     id: "news-persona", class: "input", required: true,
                                     aria_describedby: personas.data.as_ref().is_some_and(Vec::is_empty).then_some("news-persona-help"),
                                     value: "{persona.cloned().map(|id| id.to_string()).unwrap_or_default()}",
                                     onchange: move |e| persona.set(e.value().parse().ok()),
                                     option { value: "", "Select persona" }
                                     if let Some(items) = personas.data.as_ref() {
                                         for p in items {
                                             option { key: "{p.id}", value: "{p.id}", "{p.name}" }
                                         }
                                     }
                                 }
                                 if personas.data.as_ref().is_some_and(Vec::is_empty) {
                                     p { class: "form-help", id: "news-persona-help", "Add an individual in Projects first." }
                                 }
                             }
                            div { class: "form-row",
                                div { class: "form-field",
                                    label { class: "label-tech", r#for: "news-angle", "Angle" }
                                    input {
                                        id: "news-angle", class: "input",
                                        placeholder: "What to say", value: "{angle}", oninput: move |e| angle.set(e.value()),
                                    }
                                }
                                div { class: "form-field",
                                    label { class: "label-tech", r#for: "news-audience", "Audience" }
                                    input {
                                        id: "news-audience", class: "input",
                                        placeholder: "Who should read it", value: "{audience}", oninput: move |e| audience.set(e.value()),
                                    }
                                }
                            }
                            div { class: "form-row",
                                div { class: "form-field",
                                    label { class: "label-tech", r#for: "news-copy", "Copy direction" }
                                    textarea {
                                        id: "news-copy", class: "input", rows: "2", style: "min-height: 0; overflow-wrap: anywhere;",
                                        placeholder: "Tone, structure, claims to avoid",
                                        value: "{copy}", oninput: move |e| copy.set(e.value()),
                                    }
                                }
                                div { class: "form-field",
                                    label { class: "label-tech", r#for: "news-visual", "Visual direction" }
                                    textarea {
                                        id: "news-visual", class: "input", rows: "2", style: "min-height: 0; overflow-wrap: anywhere;",
                                        placeholder: "Composition, palette, references",
                                        value: "{visual}", oninput: move |e| visual.set(e.value()),
                                    }
                                }
                            }
                            div { class: "row",
                                button {
                                    class: "btn primary", r#type: "submit",
                                    disabled: pending() || form_for() != Some(item.id) || persona().is_none() || title.read().trim().is_empty(),
                                    if pending() { "Creating brief…" } else { "Create brief" }
                                }
                            }
                        }
                    }
                    Panel { title: "Sources and evidence".to_string(), caption: None,
                        if !item.sources.is_empty() {
                            div { class: "row",
                                for source in item.sources.iter() {
                                    span { class: "chip", style: "max-width: 100%; white-space: normal; overflow-wrap: anywhere;", key: "{source}",
                                         if source == &item.canonical_url { "Primary source" } else { "{news_source(source)}" }
                                    }
                                }
                            }
                        }
                        if item.evidence.is_empty() {
                            p { class: "muted", "No quoted evidence attached." }
                        } else {
                            for (index, evidence) in item.evidence.iter().enumerate() {
                                 {
                                     let source = if evidence.url == item.canonical_url { "Primary source" } else { news_source(&evidence.url) };
                                     if evidence.quote.trim() == item.summary.trim() {
                                         rsx! { p { class: "form-help", style: "overflow-wrap: anywhere;", key: "{index}", "Summary source: {source}" } }
                                     } else {
                                         rsx! { blockquote { class: "quote", style: "overflow-wrap: anywhere;", key: "{index}",
                                             "“{evidence.quote}”"
                                             cite { "{source}" }
                                         } }
                                     }
                                 }
                            }
                        }
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{coverage_label, news_source};

    #[test]
    fn coverage_labels_mentions_and_sources_independently() {
        assert_eq!(coverage_label(2, 1), "2 mentions · 1 source");
        assert_eq!(coverage_label(1, 2), "1 mention · 2 sources");
        assert_eq!(coverage_label(0, 0), "0 mentions · 0 sources");
    }

    #[test]
    fn source_labels_describe_the_feed_without_hiding_unknown_sources() {
        assert_eq!(news_source("n8n:rss-studio"), "Studio RSS feed");
        assert_eq!(news_source("windmill:f/news/scrape"), "Windmill news scraper");
        assert_eq!(news_source("Independent Gazette"), "Independent Gazette");
    }
}
