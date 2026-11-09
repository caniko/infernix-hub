//! Right-hand inspector: technical ids, foreign ids (paperclip issue/approval,
//! prompt ids, provider ids), the release payload hash, raw JSON and history.
//! The donor had no inspector; this is new hub chrome around the same
//! `kv` / `raw` presentation vocabulary.

use dioxus::prelude::*;

use crate::components::history::{HistoryFeed, HistoryLine};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InspectorData {
    pub title: String,
    pub ids: Vec<(String, String)>,
    pub foreign: Vec<(String, String)>,
    pub payload_hash: Option<String>,
    pub raw: String,
    pub history: Vec<HistoryLine>,
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
