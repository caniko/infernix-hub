//! Ctrl+K command palette: fuzzy actions, navigation, and live `/v1/search`
//! hits. Keyboard: ↑/↓ move, Enter runs, Esc closes.

use std::time::Duration;

use contracts::client::HubClient;
use dioxus::prelude::*;

use crate::{
    actions::{self, Action, PaletteEntry},
    api,
    format::fuzzy_filter,
    state::Store,
};

#[component]
pub fn CommandPalette(on_action: EventHandler<Action>, on_close: EventHandler<()>) -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut query = use_signal(String::new);
    let mut active = use_signal(|| 0usize);

    let mut on_input = move |value: String| {
        query.set(value.clone());
        active.set(0);
        let client = client.clone();
        spawn(async move {
            tokio::time::sleep(Duration::from_millis(180)).await;
            if query.cloned() != value {
                return;
            }
            api::load_search(store, client, value).await;
        });
    };

    let needle = query.cloned();
    let all_actions = actions::static_entries();
    let labels: Vec<&str> = all_actions.iter().map(|entry| entry.label.as_str()).collect();
    let mut entries: Vec<PaletteEntry> =
        fuzzy_filter(&needle, &labels).into_iter().filter_map(|index| all_actions.get(index).cloned()).collect();
    if !needle.trim().is_empty()
        && let Some(hits) = (store.search).cloned().data
    {
        entries.extend(hits.iter().map(actions::search_entry));
    }

    let selected = if entries.is_empty() { 0 } else { active.cloned().min(entries.len() - 1) };
    let key_entries = entries.clone();
    let mut rows: Vec<(bool, PaletteEntry)> = Vec::new();
    let mut previous_kind = "";
    for entry in entries.iter() {
        let show_header = entry.kind != previous_kind;
        previous_kind = entry.kind;
        rows.push((show_header, entry.clone()));
    }

    let search_error = (store.search).cloned().error;

    rsx! {
        div {
            class: "scrim",
            onclick: move |_| on_close.call(()),
            section {
                class: "palette",
                aria_label: "Command palette",
                onclick: move |e| e.stop_propagation(),
                label { class: "sr-only", r#for: "palette-input", "Search commands and hub content" }
                input {
                    id: "palette-input",
                    class: "input palette-input",
                    placeholder: "Search commands, content, personas, threads…",
                    autofocus: true,
                    value: "{query}",
                    oninput: move |e| on_input(e.value()),
                    onkeydown: move |e| match e.key() {
                        Key::Named(NamedKey::ArrowDown) => {
                            if !key_entries.is_empty() {
                                active.set((selected + 1) % key_entries.len());
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if !key_entries.is_empty() {
                                active.set((selected + key_entries.len() - 1) % key_entries.len());
                            }
                        }
                        Key::Named(NamedKey::Enter) => {
                            if let Some(entry) = key_entries.get(selected) {
                                on_action.call(entry.action.clone());
                            }
                        }
                        Key::Named(NamedKey::Escape) => {
                            e.stop_propagation();
                            on_close.call(());
                        }
                        _ => {}
                    },
                }
                div { class: "palette-list",
                    if rows.is_empty() {
                        p { class: "palette-empty",
                            if (store.search).cloned().loading && !needle.trim().is_empty() {
                                "Searching the hub…"
                            } else {
                                "No matching commands."
                            }
                        }
                    }
                    for (i, (show_header, entry)) in rows.iter().enumerate() {
                        if *show_header {
                            div { class: "palette-group label-tech", key: "group-{entry.kind}",
                                if entry.kind == "search" { "Search results" } else if entry.kind == "command" { "Commands" } else { "Navigate" }
                            }
                        }
                        {
                            let action = entry.action.clone();
                            rsx! {
                        button {
                            key: "{i}-{entry.label}",
                            class: if i == selected { "palette-item active" } else { "palette-item" },
                            onmouseenter: move |_| active.set(i),
                            onclick: move |_| on_action.call(action.clone()),
                            span { class: "search-hit",
                                span { "{entry.label}" }
                                span { class: "label-tech", "{entry.hint}" }
                            }
                            span { class: "p-hint", "{entry.kind}" }
                        }
                            }
                        }
                    }
                }
                if let Some(error) = search_error {
                    p { class: "palette-empty", "Search failed: {error.code} — {error.message}" }
                }
            }
        }
    }
}
