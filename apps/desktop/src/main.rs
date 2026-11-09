//! infernix-hub desktop console.
//!
//! Milestone C: the donor-derived shell now runs on real hub data through
//! `hub_contracts::client::HubClient`. The WebSocket bridge is gone; the app
//! keeps the donor's 1.5s refresh cadence and its component vocabulary.

mod actions;
mod api;
mod components;
mod format;
mod icons;
mod inspector;
mod labels;
mod palette;
mod shell;
mod state;
mod theme;
mod workspaces;

use std::time::Duration;

use dioxus::prelude::*;

pub use state::Workspace;
use state::{Loaded, Store};

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let client = use_hook(api::make_client);
    let store = Store {
        workspace: use_signal(|| Workspace::Today),
        status: use_signal(Loaded::default),
        today: use_signal(Loaded::default),
        news: use_signal(Loaded::default),
        personas: use_signal(Loaded::default),
        workflows: use_signal(Loaded::default),
        accounts: use_signal(Loaded::default),
        content: use_signal(Loaded::default),
        intents: use_signal(Loaded::default),
        relationships: use_signal(Loaded::default),
        assets: use_signal(Loaded::default),
        content_detail: use_signal(Loaded::default),
        relationship_detail: use_signal(Loaded::default),
        canon: use_signal(Loaded::default),
        chat_content: use_signal(Loaded::default),
        chat_persona: use_signal(Loaded::default),
        search: use_signal(Loaded::default),
        selected_content: use_signal(|| None),
        selected_news: use_signal(|| None),
        selected_persona: use_signal(|| None),
        selected_relationship: use_signal(|| None),
        focus_proposal: use_signal(|| None),
        asset_images: use_signal(std::collections::HashMap::new),
        release_states: use_signal(std::collections::HashMap::new),
        release_content: use_signal(std::collections::HashMap::new),
        release_indexed: use_signal(std::collections::HashSet::new),
        notice: use_signal(|| None),
        inspector_open: use_signal(|| true),
        nav_open: use_signal(|| false),
        palette_open: use_signal(|| false),
        poll_busy: use_signal(|| false),
    };
    use_context_provider(|| client.clone());
    use_context_provider(|| store);

    // Poll status + base lists + the selected detail on the donor cadence.
    let poll_client = client.clone();
    use_future(move || {
        let client = poll_client.clone();
        async move {
            api::refresh_base(store, client.clone()).await;
            api::refresh_selected(store, client.clone()).await;
            loop {
                tokio::time::sleep(Duration::from_millis(api::POLL_INTERVAL_MS)).await;
                api::refresh_base(store, client.clone()).await;
                api::refresh_selected(store, client.clone()).await;
            }
        }
    });

    // Detail loads follow the selections.
    let content_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_content).cloned() {
            let client = content_client.clone();
            spawn(api::load_content_detail(store, client, id));
        }
    });
    let relationship_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_relationship).cloned() {
            let client = relationship_client.clone();
            spawn(api::load_relationship_detail(store, client, id));
        }
    });
    let canon_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_persona).cloned() {
            let client = canon_client.clone();
            spawn(api::load_canon(store, client, id));
        }
    });
    let content_chat_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_content).cloned() {
            let client = content_chat_client.clone();
            spawn(api::load_chat(store, client, "content", id));
        }
    });
    let persona_chat_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_persona).cloned() {
            let client = persona_chat_client.clone();
            spawn(api::load_chat(store, client, "persona", id));
        }
    });

    // Global shortcuts live in the webview so they also work while a form field
    // is focused. The JS only reports intent; all state changes happen here.
    use_future(move || async move {
        let mut eval = document::eval(
            r#"
            document.addEventListener('keydown', (event) => {
                const target = event.target;
                const editing = target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA'
                    || target.tagName === 'SELECT' || target.isContentEditable);
                if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
                    event.preventDefault();
                    dioxus.send('palette');
                    return;
                }
                if (event.key === 'Escape') {
                    dioxus.send('escape');
                    return;
                }
                if (!editing && !event.ctrlKey && !event.metaKey && !event.altKey && /^[1-6]$/.test(event.key)) {
                    dioxus.send('ws:' + event.key);
                }
            });
            "#,
        );
        while let Ok(message) = eval.recv::<String>().await {
            match message.as_str() {
                    "palette" => store.set_palette_open(true),
                    "escape" => {
                        if (store.palette_open).cloned() {
                            store.set_palette_open(false);
                        } else if (store.inspector_open).cloned() {
                            store.set_inspector_open(false);
                        } else {
                            store.set_nav_open(false);
                        }
                    }
                    "ws:1" => store.set_workspace(Workspace::Today),
                    "ws:2" => store.set_workspace(Workspace::News),
                    "ws:3" => store.set_workspace(Workspace::Studio),
                    "ws:4" => store.set_workspace(Workspace::Calendar),
                    "ws:5" => store.set_workspace(Workspace::Relationships),
                    "ws:6" => store.set_workspace(Workspace::Library),
                _ => {}
            }
        }
    });

    let status = (store.status).cloned();
    let live = status.data.is_some() && status.error.is_none();
    let status_label = if live {
        "hub online"
    } else if status.error.is_some() {
        "hub offline"
    } else {
        "connecting…"
    }
    .to_string();
    let fixture = status.data.as_ref().is_some_and(|status| status.fixture_mode);
    let mut segments: Vec<shell::StatusSeg> = status
        .data
        .as_ref()
        .map(|status| {
            status
                .integrations
                .iter()
                .map(|integration| {
                    let (mode_label, mode_tone) = labels::adapter_mode(integration.mode);
                    let (verification_label, _) = labels::verification(integration.verification);
                    shell::StatusSeg {
                        label: format!("{} · {} · {}", integration.name, mode_label, verification_label),
                        tone: if mode_tone == "ok" { "live".to_string() } else { mode_tone.to_string() },
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if status.data.as_ref().is_some_and(|status| status.production_paused) {
        segments.push(shell::StatusSeg { label: "PRODUCTION PAUSED".into(), tone: "warn".into() });
    }
    let api_version = status
        .data
        .as_ref()
        .map(|status| status.api_version.clone())
        .unwrap_or_else(|| "v1".into());

    let inspector_data: Option<inspector::InspectorData> = match (store.workspace).cloned() {
        Workspace::Studio => (store.content_detail).cloned()
            .data
            .as_ref()
            .filter(|detail| Some(detail.item.id) == (store.selected_content).cloned())
            .map(inspector::from_content),
        Workspace::Relationships => (store.relationship_detail).cloned()
            .data
            .as_ref()
            .filter(|detail| Some(detail.relationship.id) == (store.selected_relationship).cloned())
            .map(inspector::from_relationship),
        Workspace::Library => {
            let canon = (store.canon).cloned();
            (store.personas).cloned()
                .data
                .as_ref()
                .and_then(|personas| {
                    personas
                        .iter()
                        .find(|persona| Some(persona.id) == (store.selected_persona).cloned())
                })
                .map(|persona| {
                    inspector::from_persona(persona, canon.data.as_deref().unwrap_or(&[]))
                })
        }
        Workspace::News => (store.news).cloned()
            .data
            .as_ref()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| Some(item.id) == (store.selected_news).cloned())
            })
            .map(inspector::from_news),
        _ => None,
    };

    let action_client = client.clone();
    let on_action = move |action: actions::Action| {
        store.set_palette_open(false);
        match action {
            actions::Action::Workspace(workspace) => store.set_workspace(workspace),
            actions::Action::ToggleInspector => {
                store.toggle_inspector()
            }
            actions::Action::Refresh => {
                let client = action_client.clone();
                spawn(async move {
                    api::refresh_base(store, client.clone()).await;
                    api::refresh_selected(store, client).await;
                });
            }
            actions::Action::OpenSubject(subject_type, id) => {
                actions::open_subject(store, &subject_type, id)
            }
        }
    };

    rsx! {
        document::Title { "infernix hub" }
        style { dangerous_inner_html: theme::style_sheet() }
        div { class: "app",
            shell::Sidebar { workspace: store.workspace, nav_open: store.nav_open }
            div { class: "main",
                shell::Topbar {
                    workspace: (store.workspace).cloned(),
                    inspector_open: store.inspector_open,
                    nav_open: store.nav_open,
                    live,
                    status_label,
                    on_palette: move |_| store.set_palette_open(true),
                }
                if fixture {
                    shell::FixtureBanner {}
                }
                if let Some(notice) = (store.notice).cloned() {
                    div {
                        class: if notice.ok { "notice-bar ok" } else { "notice-bar err" },
                        role: "status",
                        span { "{notice.text}" }
                        button {
                            class: "icon-btn",
                            aria_label: "Dismiss notice",
                            onclick: move |_| store.clear_notice(),
                            "✕"
                        }
                    }
                }
                div { class: if (store.inspector_open).cloned() { "stage" } else { "stage inspector-closed" },
                    match (store.workspace).cloned() {
                        Workspace::Today => rsx! { workspaces::today::TodayView {} },
                        Workspace::News => rsx! { workspaces::news::NewsView {} },
                        Workspace::Studio => rsx! { workspaces::studio::StudioView {} },
                        Workspace::Calendar => rsx! { workspaces::calendar::CalendarView {} },
                        Workspace::Relationships => rsx! { workspaces::relationships::RelationshipsView {} },
                        Workspace::Library => rsx! { workspaces::library::LibraryView {} },
                    }
                    if (store.inspector_open).cloned() {
                        inspector::Inspector {
                            data: inspector_data,
                            on_close: move |_| store.set_inspector_open(false),
                        }
                    }
                }
            }
            shell::StatusBar {
                connected: live,
                items: segments,
                right: format!("API {api_version} · INFERNIX-HUB v0.1.0"),
            }
            if (store.palette_open).cloned() {
                palette::CommandPalette {
                    on_action,
                    on_close: move |_| store.set_palette_open(false),
                }
            }
        }
    }
}
