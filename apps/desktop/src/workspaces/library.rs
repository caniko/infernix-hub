//! The asset library.

use contracts::client::HubClient;
use dioxus::prelude::*;

use crate::{
    actions, api,
    components::{Chip, EmptyState, ErrorCard, Loading},
    format, labels,
    state::{Store, Workspace},
};

#[component]
pub fn LibraryView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut search = use_signal(String::new);
    let mut filter = use_signal(|| "");
    let assets = (store.assets).cloned();
    let content = (store.content).cloned();
    let previews = store.asset_images.read();
    use_effect(move || {
        let Some(assets) = (store.assets).cloned().data else { return };
        let client = client.clone();
        spawn(async move {
            api::load_asset_bytes(store, client, &assets).await;
        });
    });
    rsx! {
        main { class: "work",
            div { class: "work-head", h1 { "Generated media" } }
            if let Some(error) = assets.error {
                ErrorCard { code: error.code, message: error.message }
            }
            match assets.data {
                None if assets.loading => rsx! { Loading { text: "Loading assets…".to_string() } },
                None => rsx! {},
                Some(assets) if assets.is_empty() => rsx! {
                    EmptyState {
                        title: "No assets yet".to_string(),
                        hint: Some("Assets appear here after a successful generation in Studio.".to_string()),
                        actions: rsx! {
                            button { class: "btn primary", onclick: move |_| store.set_workspace(Workspace::Studio), "Generate assets in Studio" }
                        },
                    }
                },
                Some(assets) => {
                    let selected_filter = filter();
                    let query = search.read().trim().to_lowercase();
                    let matching: Vec<_> = assets.iter().filter_map(|asset| {
                        let kind = if format::is_video(&asset.mime) {
                            "Video"
                        } else if asset.mime.trim().split_once('/').is_some_and(|(kind, _)| kind.eq_ignore_ascii_case("image")) {
                            "Image"
                        } else {
                            "Asset"
                        };
                        if !selected_filter.is_empty() && selected_filter != kind {
                            return None;
                        }
                        let title = asset.content_item_id.and_then(|id| {
                            content.data.as_ref()?.iter().find(|item| item.id == id).map(|item| item.title.as_str())
                        });
                        let alt = asset.alt_text.trim();
                        let name = if alt.is_empty() { title.unwrap_or(kind) } else { alt };
                        if !query.is_empty() && !name.to_lowercase().contains(&query)
                            && !title.is_some_and(|title| title.to_lowercase().contains(&query)) {
                            return None;
                        }
                        Some((asset, kind, name, title.filter(|title| *title != name)))
                    }).collect();
                    rsx! {
                        div { class: "toolbar",
                            label { class: "sr-only", r#for: "asset-search", "Search assets" }
                            input {
                                id: "asset-search", class: "input", r#type: "search", style: "width: min(100%, 16rem);",
                                placeholder: "Search title or alt text", value: "{search}", oninput: move |e| search.set(e.value()),
                            }
                            div { class: "row", role: "group", aria_label: "Asset type",
                                for (label, value) in [("All", ""), ("Images", "Image"), ("Videos", "Video")] {
                                    button { class: "filter-chip", key: "{label}", aria_pressed: selected_filter == value,
                                        onclick: move |_| filter.set(value), "{label}"
                                    }
                                }
                            }
                            span { class: "badge-count", role: "status", "{matching.len()} of {assets.len()}" }
                        }
                        if matching.is_empty() {
                            EmptyState { title: "No matching assets".to_string(), hint: Some("Try another search or asset type.".to_string()) }
                        } else {
                            div { class: "asset-grid four",
                                for (asset, kind, name, title) in matching {
                                    {
                                        let (state_label, state_tone) = labels::asset_state(asset.state);
                                        let image = previews.get(&asset.id);
                                        let content_id = asset.content_item_id;
                                        rsx! {
                                            article { class: "glass-card asset-tile", key: "{asset.id}",
                                                h2 { style: "font-size: 14px; overflow-wrap: anywhere;", "{name}" }
                                                if let Some(title) = title {
                                                    p { class: "form-help", "{title}" }
                                                }
                                                div { class: "asset-frame",
                                                    match (image, kind) {
                                                        (Some(uri), "Video") => rsx! {
                                                            video { class: "asset-video", aria_label: "{name}", controls: true, src: "{uri}", "Video preview unavailable." }
                                                        },
                                                        (Some(uri), "Image") => rsx! { img { src: "{uri}", alt: "{asset.alt_text}" } },
                                                        _ => rsx! { span { class: "asset-missing", "Preview unavailable" } },
                                                    }
                                                }
                                                div { class: "asset-meta",
                                                    Chip { label: state_label.to_string(), tone: Some(state_tone.to_string()) }
                                                    if asset.selected { span { class: "chip sage", "Selected" } }
                                                }
                                                div { class: "row",
                                                    if let Some(uri) = image {
                                                        a { class: "btn secondary btn-xs", href: "{uri}", download: "{format::media_name(&asset.mime, &asset.sha256)}", "Download" }
                                                    }
                                                    if let Some(id) = content_id {
                                                        button { class: "btn secondary btn-xs", onclick: move |_| actions::open_subject(store, "content", id), "Open in Studio" }
                                                    }
                                                }
                                            }
                                        }
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
