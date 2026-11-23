//! Ctrl+K command palette: fuzzy actions, navigation, and live `/v1/search`
//! hits. Keyboard: ↑/↓ move, Enter runs, Esc closes.

use std::time::Duration;

use contracts::{SearchHit, client::HubClient};
use dioxus::prelude::*;

use crate::{
    actions::{self, Action, PaletteEntry},
    api,
    format::fuzzy_filter,
    state::Loaded,
};

#[derive(Default)]
struct PaletteSearch {
    generation: u64,
    result: Loaded<Vec<SearchHit>>,
}

impl PaletteSearch {
    fn begin(&mut self, will_search: bool) -> u64 {
        self.generation += 1;
        self.result = if will_search { Loaded::default() } else { Loaded::ready(Vec::new()) };
        self.generation
    }

    fn complete(&mut self, generation: u64, result: Loaded<Vec<SearchHit>>) {
        if generation == self.generation {
            self.result = result;
        }
    }
}

fn close_dialog() {
    let _ = document::eval("document.getElementById('command-palette')?.close()");
}

fn move_to(mut active: Signal<usize>, index: usize) {
    active.set(index);
    let _ = document::eval(&format!("document.getElementById('palette-option-{index}')?.scrollIntoView({{block: 'nearest'}})"));
}

#[component]
pub fn CommandPalette(inspector_available: bool, on_action: EventHandler<Action>, on_close: EventHandler<()>) -> Element {
    let client = use_context::<HubClient>();
    let mut query = use_signal(String::new);
    let mut active = use_signal(|| 0usize);
    let mut search = use_signal(PaletteSearch::default);

    let mut on_input = move |value: String| {
        let will_search = value.trim().len() >= 2;
        let generation = search.write().begin(will_search);
        query.set(value.clone());
        active.set(0);
        let _ = document::eval("document.getElementById('palette-scroll').scrollTop = 0");
        if !will_search {
            return;
        }
        let client = client.clone();
        spawn(async move {
            tokio::time::sleep(Duration::from_millis(180)).await;
            if search.peek().generation != generation {
                return;
            }
            let result = match api::timed(client.search(&value)).await {
                Ok(hits) => Loaded::ready(hits),
                Err(error) => Loaded { data: None, error: Some(error), loading: false },
            };
            search.write().complete(generation, result);
        });
    };

    let needle = query.cloned();
    let mut all_actions = actions::static_entries();
    all_actions.retain(|entry| inspector_available || entry.action != Action::ToggleInspector);
    let labels: Vec<&str> = all_actions.iter().map(|entry| entry.label.as_str()).collect();
    let mut entries: Vec<PaletteEntry> =
        fuzzy_filter(&needle, &labels).into_iter().filter_map(|index| all_actions.get(index).cloned()).collect();
    // Match order stays stable within each group; keyboard order matches DOM order.
    entries.sort_by_key(|entry| entry.kind == "command");
    let (generation, result) = {
        let state = search.read();
        (state.generation, state.result.clone())
    };
    if let Some(hits) = result.data {
        entries.extend(hits.iter().map(actions::search_entry));
    }

    let selected = active.cloned().min(entries.len().saturating_sub(1));
    let active_id = (!entries.is_empty()).then(|| format!("palette-option-{selected}"));
    let key_entries = entries.clone();
    let run = move |action: Action| {
        // Ignore events queued for options rendered before the latest edit.
        if search.peek().generation == generation {
            close_dialog();
            on_action.call(action);
        }
    };
    let dismiss = move || {
        close_dialog();
        on_close.call(());
    };

    rsx! {
        dialog {
            id: "command-palette",
            class: "palette-dialog",
            role: "dialog",
            aria_modal: "true",
            aria_label: "Command palette",
            onmounted: move |_| {
                let _ = document::eval("document.getElementById('command-palette')?.showModal()");
            },
            oncancel: move |e| {
                e.prevent_default();
                dismiss();
            },
            onclick: move |_| dismiss(),
            div {
                class: "palette",
                onclick: move |e| e.stop_propagation(),
                header { class: "palette-header",
                    label { r#for: "palette-input", "Search commands" }
                    input {
                        id: "palette-input",
                        class: "input palette-input",
                        role: "combobox",
                        aria_autocomplete: "list",
                        aria_controls: "palette-options",
                        aria_expanded: "true",
                        aria_activedescendant: active_id,
                        placeholder: "Search commands, workspaces, hosts, content, personas, or threads",
                        autofocus: true,
                        value: "{query}",
                        oninput: move |e| on_input(e.value()),
                        onkeydown: move |e| match e.key() {
                            Key::Named(NamedKey::ArrowDown) => {
                                e.prevent_default();
                                if !key_entries.is_empty() {
                                    move_to(active, (selected + 1) % key_entries.len());
                                }
                            }
                            Key::Named(NamedKey::ArrowUp) => {
                                e.prevent_default();
                                if !key_entries.is_empty() {
                                    move_to(active, (selected + key_entries.len() - 1) % key_entries.len());
                                }
                            }
                            Key::Named(NamedKey::Enter) => {
                                e.prevent_default();
                                if let Some(entry) = key_entries.get(selected) {
                                    run(entry.action.clone());
                                }
                            }
                            Key::Named(NamedKey::Escape) => {
                                e.prevent_default();
                                e.stop_propagation();
                                dismiss();
                            }
                            _ => {}
                        },
                    }
                    button {
                        class: "icon-btn palette-close",
                        aria_label: "Close command palette",
                        onclick: move |_| dismiss(),
                        "×"
                    }
                }
                div { class: "palette-list", id: "palette-scroll",
                    div { id: "palette-options", role: "listbox", aria_label: "Matching commands",
                        for (kind, title) in [("navigate", "Navigation"), ("command", "Commands"), ("search", "Search results")] {
                            if entries.iter().any(|entry| entry.kind == kind) {
                                div { key: "{kind}", role: "group", aria_label: "{title}",
                                    div { class: "palette-group", aria_hidden: "true", "{title}" }
                                    for (i, entry) in entries.iter().enumerate().filter(|(_, entry)| entry.kind == kind) {
                                        {
                                            let action = entry.action.clone();
                                            rsx! {
                                                div {
                                                    key: "{i}-{entry.label}",
                                                    id: "palette-option-{i}",
                                                    role: "option",
                                                    aria_selected: if i == selected { "true" } else { "false" },
                                                    class: if i == selected { "palette-item active" } else { "palette-item" },
                                                    onmousedown: move |e| e.prevent_default(),
                                                    onmousemove: move |_| {
                                                        if *active.peek() != i {
                                                            active.set(i);
                                                        }
                                                    },
                                                    onclick: move |_| run(action.clone()),
                                                    span { class: "search-hit",
                                                        span { "{entry.label}" }
                                                        span { class: "palette-description", "{entry.hint}" }
                                                    }
                                                    if let Action::Workspace(workspace) = &entry.action {
                                                        kbd { class: "kbd-cap", "{workspace.key()}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(error) = result.error {
                        p { class: "palette-empty", role: "status", "Search failed: {error.message}" }
                    } else if entries.is_empty() {
                        p { class: "palette-empty", role: "status",
                            if result.loading && needle.trim().len() >= 2 {
                                "Searching the hub…"
                            } else {
                                "No matching commands."
                            }
                        }
                    }
                }
                footer { class: "palette-legend",
                    span { kbd { class: "kbd-cap", "↑↓" } " Move" }
                    span { kbd { class: "kbd-cap", "↵" } " Open" }
                    span { kbd { class: "kbd-cap", "Esc" } " Close" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_latest_search_can_supply_remote_options() {
        let hits = |title: &str| {
            Loaded::ready(vec![SearchHit {
                subject_type: "news".into(),
                subject_id: uuid::Uuid::nil(),
                title: title.into(),
                snippet: String::new(),
                rank: 0.0,
            }])
        };
        let mut search = PaletteSearch::default();
        let first = search.begin(true);
        search.complete(first, hits("Previous query"));
        let second = search.begin(true);
        assert!(search.result.data.is_none());
        let latest = search.begin(true); // The user can type the first query again.
        search.complete(latest, hits("Current query"));
        search.complete(first, hits("Obsolete first query"));
        search.complete(second, hits("Obsolete second query"));
        assert_eq!(search.result, hits("Current query"));

        search.begin(false); // Clearing the input also invalidates in-flight work.
        search.complete(latest, hits("Cleared query"));
        assert_eq!(search.result, Loaded::ready(vec![]));
    }
}
