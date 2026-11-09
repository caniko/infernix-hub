//! News: coverage + evidence from `/v1/news`, and brief creation into Studio.

use contracts::{Brief, CreateBriefIn, NewsItem};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    components::{EmptyState, ErrorCard, Loading, Panel},
    format,
    shell::ContextItem,
    state::{Store, Workspace},
};

#[component]
pub fn NewsView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<contracts::client::HubClient>();
    let news = (store.news).cloned();
    let selected_id = (store.selected_news).cloned();
    let selected: Option<NewsItem> = news
        .data
        .as_ref()
        .and_then(|items| items.iter().find(|item| Some(item.id) == selected_id).cloned());

    let mut persona = use_signal(|| Option::<Uuid>::None);
    let mut title = use_signal(String::new);
    let mut angle = use_signal(String::new);
    let mut audience = use_signal(String::new);
    let mut copy = use_signal(String::new);
    let mut visual = use_signal(String::new);
    let mut form_for = use_signal(|| Option::<Uuid>::None);

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
        if persona.cloned().is_none() {
            persona.set((store.personas).cloned().data.and_then(|items| items.first().map(|p| p.id)));
        }
    });

    let submit = move |_| {
        let Some(persona_id) = persona.cloned() else {
            store.notify(false, "Select a persona before creating a brief.");
            return;
        };
        let Some(news_item_id) = (store.selected_news).cloned() else {
            store.notify(false, "Select a news item first.");
            return;
        };
        let title_value = title.cloned().trim().to_string();
        if title_value.is_empty() {
            store.notify(false, "A brief needs a title.");
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
            },
        };
        let client = client.clone();
        spawn(async move {
            match api::timed(client.create_brief(&body)).await {
                Ok(item) => {
                    store.notify(true, format!("Brief created: {}", item.title));
                    store.set_selected_content(Some(item.id));
                    store.set_workspace(Workspace::Studio);
                    api::refresh_base(store, client).await;
                }
                Err(error) => store.notify(false, format!("{} — {}", error.code, error.message)),
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
                                meta: format!("coverage {} · {} sources", item.coverage, item.sources.len()),
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
                None if news.error.is_none() => rsx! { Loading { text: "Loading news…".to_string() } },
                None => rsx! { EmptyState { title: "Select a news item.".to_string(), hint: None } },
                Some(item) => rsx! {
                    div { class: "work-head",
                        div {
                            h1 { "{item.title}" }
                            p { class: "hint", "coverage {item.coverage} · {item.sources.len()} sources · first seen {format::fmt_dt(item.first_seen_at)}" }
                        }
                        div { class: "spacer" }
                        button {
                            class: "btn btn-glass",
                            onclick: move |_| api::open_external(&item.canonical_url),
                            "Open source"
                        }
                    }
                    Panel { title: "Summary".to_string(), caption: Some("normalized event".to_string()),
                        p { "{item.summary}" }
                        p { class: "label-tech", "{item.canonical_url}" }
                        div { class: "row",
                            for source in item.sources.iter() {
                                span { class: "chip", key: "{source}", "{source}" }
                            }
                            span { class: "chip info", "coverage {item.coverage}" }
                            if let Some(content) = item.content_item_id {
                                button {
                                    class: "btn btn-glass btn-xs",
                                    onclick: move |_| crate::actions::open_subject(store, "content", content),
                                    "Open content"
                                }
                            }
                        }
                    }
                    Panel { title: "Evidence".to_string(), caption: Some("quoted sources".to_string()),
                        if item.evidence.is_empty() {
                            EmptyState { title: "No evidence quotes attached.".to_string(), hint: Some("Ingest can attach quotes to the event.".to_string()) }
                        } else {
                            for (i, evidence) in item.evidence.iter().enumerate() {
                                blockquote { class: "quote", key: "{i}",
                                    "“{evidence.quote}”"
                                    cite { "{evidence.url}" }
                                }
                            }
                        }
                    }
                    Panel { title: "Create brief".to_string(), caption: Some("POST /v1/content".to_string()),
                        div { class: "form-grid",
                            div { class: "field-row",
                                label { class: "label-tech", r#for: "news-persona", "Persona" }
                                div {
                                    if let Some(error) = (store.personas).cloned().error {
                                        ErrorCard { code: error.code, message: error.message }
                                    } else if (store.personas).cloned().data.is_none() {
                                        Loading { text: "Loading personas…".to_string() }
                                    }
                                    select {
                                    id: "news-persona",
                                    class: "input",
                                    value: "{persona.cloned().map(|id| id.to_string()).unwrap_or_default()}",
                                    onchange: move |e| persona.set(e.value().parse().ok()),
                                    option { value: "", "Select persona" }
                                    if let Some(personas) = (store.personas).cloned().data {
                                        for p in personas {
                                            option { key: "{p.id}", value: "{p.id}", "{p.name}" }
                                        }
                                    }
                                    }
                                }
                            }
                            div { class: "field-row",
                                label { class: "label-tech", r#for: "news-title", "Title" }
                                input {
                                    id: "news-title", class: "input", value: "{title}",
                                    oninput: move |e| title.set(e.value()),
                                }
                            }
                            div { class: "field-row",
                                label { class: "label-tech", r#for: "news-angle", "Angle" }
                                input {
                                    id: "news-angle", class: "input",
                                    placeholder: "what the operator wants to say",
                                    value: "{angle}", oninput: move |e| angle.set(e.value()),
                                }
                            }
                            div { class: "field-row",
                                label { class: "label-tech", r#for: "news-audience", "Audience" }
                                input {
                                    id: "news-audience", class: "input",
                                    placeholder: "who it is for",
                                    value: "{audience}", oninput: move |e| audience.set(e.value()),
                                }
                            }
                            div { class: "field-row",
                                label { class: "label-tech", r#for: "news-copy", "Copy direction" }
                                textarea {
                                    id: "news-copy", class: "input",
                                    placeholder: "tone, structure, claims to avoid",
                                    value: "{copy}", oninput: move |e| copy.set(e.value()),
                                }
                            }
                            div { class: "field-row",
                                label { class: "label-tech", r#for: "news-visual", "Visual direction" }
                                textarea {
                                    id: "news-visual", class: "input",
                                    placeholder: "composition, palette, references",
                                    value: "{visual}", oninput: move |e| visual.set(e.value()),
                                }
                            }
                            div { class: "form-actions",
                                button { class: "btn btn-primary", onclick: submit, "Create brief" }
                            }
                        }
                    }
                },
            }
        }
    }
}
