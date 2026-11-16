//! The asset library.

use contracts::client::HubClient;
use dioxus::prelude::*;

use crate::{
    actions, api,
    components::{Chip, EmptyState, ErrorCard, Loading},
    format, labels,
    state::Store,
};

#[component]
pub fn LibraryView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    use_effect(move || {
        let Some(assets) = (store.assets).cloned().data else { return };
        let client = client.clone();
        spawn(async move {
            api::load_asset_bytes(store, client, &assets).await;
        });
    });
    rsx! {
        main { class: "work",
            div { class: "work-head", h1 { "Library" } }
            if let Some(error) = (store.assets).cloned().error {
                ErrorCard { code: error.code, message: error.message }
            }
            match (store.assets).cloned().data {
                None if (store.assets).cloned().error.is_none() => rsx! { Loading { text: "Loading assets…".to_string() } },
                None => rsx! {},
                Some(assets) if assets.is_empty() => rsx! {
                    EmptyState { title: "No assets yet.".to_string(), hint: Some("Assets arrive with successful generations.".to_string()) }
                },
                Some(assets) => rsx! {
                    div { class: "asset-grid four",
                        for asset in assets {
                            {
                                let (state_label, state_tone) = labels::asset_state(asset.state);
                                let image = (store.asset_images).cloned().get(&asset.id).cloned();
                                let content_id = asset.content_item_id;
                                rsx! {
                                    div { class: "glass-card asset-tile", key: "{asset.id}",
                                        div { class: "asset-frame",
                                            if let Some(uri) = image.clone() {
                                                if format::is_video(&asset.mime) {
                                                    video { class: "asset-video", controls: true, src: "{uri}", "video preview unavailable ({asset.mime})" }
                                                } else {
                                                    img { src: "{uri}", alt: "{asset.alt_text}" }
                                                }
                                            } else {
                                                span { class: "asset-missing", "{state_label} · {asset.mime}" }
                                            }
                                        }
                                        div { class: "asset-meta",
                                            Chip { label: state_label.to_string(), tone: Some(state_tone.to_string()) }
                                            span { class: "chip muted", "sha {format::short_hash(&asset.sha256)}" }
                                            if asset.selected { span { class: "chip sage", "selected" } }
                                            if format::is_video(&asset.mime) {
                                                if let Some(uri) = image {
                                                    a { class: "btn btn-glass btn-xs", href: "{uri}", download: "{format::media_name(&asset.mime, &asset.sha256)}", "Download" }
                                                }
                                            }
                                        }
                                        div { class: "label-tech", "{asset.alt_text}" }
                                        if let Some(content_id) = content_id {
                                            button { class: "btn btn-glass btn-xs", onclick: move |_| actions::open_subject(store, "content", content_id), "Open content" }
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
