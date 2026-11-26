//! fleetix-hub desktop console.
//!
//! Milestone C: the donor-derived shell now runs on real hub data through
//! `hub_contracts::client::HubClient`. The WebSocket bridge is gone; the app
//! keeps the donor's 1.5s refresh cadence and its component vocabulary.

mod actions;
mod api;
mod components;
mod fleet;
mod format;
mod icons;
mod inspector;
mod labels;
mod palette;
mod platform;
mod shell;
mod state;
mod theme;
mod workspaces;

use std::time::Duration;

use dioxus::prelude::*;

pub use state::Workspace;
use state::{Loaded, Store};

fn main() {
    use dioxus::desktop::{Config, LogicalSize, WindowBuilder, tao::window::Theme};
    let window_theme = if theme::is_dark_theme() { Theme::Dark } else { Theme::Light };
    let window =
        WindowBuilder::new().with_title("fleetix hub").with_inner_size(LogicalSize::new(1440.0, 900.0)).with_theme(Some(window_theme));
    let cfg = Config::new().with_window(window).with_menu(None).with_background_color(theme::window_background_rgba());
    dioxus::LaunchBuilder::desktop().with_cfg(cfg).launch(App);
}

#[component]
fn App() -> Element {
    let client = use_hook(api::make_client);
    // `--workspace NAME` picks the start workspace (the tray passes
    // `notifications`); an unknown name keeps the default and says so.
    let start = use_hook(|| {
        let (workspace, unknown) = state::start_workspace(&std::env::args().collect::<Vec<_>>());
        if let Some(name) = unknown {
            eprintln!("--workspace {name:?} is not a workspace; starting at {}.", workspace.label());
        }
        workspace
    });
    let store = Store {
        workspace: use_signal(|| start),
        status: use_signal(Loaded::default),
        fleet: use_signal(Loaded::default),
        notifications: use_signal(Default::default),
        renewals: use_signal(Loaded::default),
        today: use_signal(Loaded::default),
        news: use_signal(Loaded::default),
        personas: use_signal(Loaded::default),
        projects: use_signal(Loaded::default),
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
        selected_content: use_signal(|| None),
        selected_news: use_signal(|| None),
        selected_signal: use_signal(|| None),
        selected_persona: use_signal(|| None),
        selected_project: use_signal(|| None),
        projects_show_individual: use_signal(|| false),
        selected_relationship: use_signal(|| None),
        selected_host: use_signal(|| None),
        focus_proposal: use_signal(|| None),
        asset_images: use_signal(std::collections::HashMap::new),
        release_states: use_signal(std::collections::HashMap::new),
        release_content: use_signal(std::collections::HashMap::new),
        release_indexed: use_signal(std::collections::HashSet::new),
        notice: use_signal(|| None),
        inspector_open: use_signal(|| false),
        nav_open: use_signal(|| false),
        palette_open: use_signal(|| false),
        poll_busy: use_signal(|| false),
    };
    use_context_provider(|| client.clone());
    use_context_provider(|| store);
    let mut narrow_nav = use_signal(|| false);
    let inspector_selection = use_memo(move || {
        let workspace = (store.workspace).cloned();
        let workspace = if workspace.available(store.status.read().data.as_ref()) { workspace } else { Workspace::Today };
        (workspace, store.inspector_item(workspace))
    });
    use_effect(move || {
        if let Some(status) = store.status.read().data.as_ref()
            && !(store.workspace)().available(Some(status))
        {
            store.set_workspace(Workspace::Today);
        }
    });
    use_effect(move || {
        let (_, selected) = inspector_selection.cloned();
        let w = &dioxus::desktop::window().window;
        // A selection opens the dock, not a narrow-screen overlay. This effect
        // only tracks identity, so polling and manually closing the pane don't rerun it.
        store.set_inspector_open(selected.is_some() && w.inner_size().to_logical::<f64>(w.scale_factor()).width > 1180.0);
    });

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

    // Notifications are a server-held long poll, not a timer: one snapshot,
    // then the change feed as it happens (the tray consumes the same feed).
    let notification_client = client.clone();
    use_future(move || {
        let client = notification_client.clone();
        async move { api::watch_notifications(store, client).await }
    });

    // Detail loads follow the selections.
    let can_load_operations = move || api::poll_operations(store.status.peek().data.as_ref());
    let content_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_content).cloned()
            && can_load_operations()
        {
            let client = content_client.clone();
            spawn(api::load_content_detail(store, client, id));
        }
    });
    let relationship_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_relationship).cloned()
            && can_load_operations()
        {
            let client = relationship_client.clone();
            spawn(api::load_relationship_detail(store, client, id));
        }
    });
    let canon_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_persona).cloned()
            && can_load_operations()
        {
            let client = canon_client.clone();
            spawn(api::load_canon(store, client, id));
        }
    });
    let content_chat_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_content).cloned()
            && can_load_operations()
        {
            let client = content_chat_client.clone();
            spawn(api::load_chat(store, client, "content", id));
        }
    });
    let persona_chat_client = client.clone();
    use_effect(move || {
        if let Some(id) = (store.selected_persona).cloned()
            && can_load_operations()
        {
            let client = persona_chat_client.clone();
            spawn(api::load_chat(store, client, "persona", id));
        }
    });

    // Global shortcuts live in the webview so they also work while a form field
    // is focused. The JS only reports intent; all state changes happen here.
    use_future(move || async move {
        let mut eval = document::eval(
            r#"
            const narrow = window.matchMedia('(max-width: 900px)');
            const report = () => dioxus.send(narrow.matches ? 'narrow' : 'wide');
            narrow.addEventListener('change', report);
            report();
            document.addEventListener('keydown', (event) => {
                const target = event.target;
                if (target?.closest('#command-palette')) return;
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
                if (!editing && !event.ctrlKey && !event.metaKey && !event.altKey && /^[0-9inpu]$/.test(event.key.toLowerCase())) {
                    dioxus.send('ws:' + event.key.toLowerCase());
                }
            });
            "#,
        );
        while let Ok(message) = eval.recv::<String>().await {
            match message.as_str() {
                "narrow" => narrow_nav.set(true),
                "wide" => {
                    narrow_nav.set(false);
                    store.set_nav_open(false);
                }
                "palette" => store.set_palette_open(true),
                "escape" => {
                    if (store.palette_open).cloned() {
                        store.set_palette_open(false);
                    } else if (store.nav_open).cloned() {
                        store.set_nav_open(false);
                        shell::focus_nav_toggle();
                    } else {
                        store.set_inspector_open(false);
                    }
                }
                // The key-to-workspace mapping lives in Workspace.
                message if message.starts_with("ws:") => {
                    if let Some(workspace) = Workspace::from_key(&message[3..]) {
                        store.set_workspace(workspace);
                    }
                }
                _ => {}
            }
        }
    });

    let status = (store.status).cloned();
    let operations = api::poll_operations(status.data.as_ref());
    let live = status.data.is_some() && status.error.is_none();
    let status_label = api::connection_label(&status).to_string();
    let fixture = status.data.as_ref().is_some_and(|status| status.fixture_mode);
    let (all_fixture, simulated_integrations): (bool, Vec<String>) = status
        .data
        .as_ref()
        .filter(|status| status.fixture_mode)
        .map(|status| {
            let simulated = status
                .integrations
                .iter()
                .filter(|integration| integration.mode == contracts::AdapterMode::Fixture)
                .map(|integration| integration.name.clone())
                .collect();
            let any_live = status.integrations.iter().any(|integration| integration.mode == contracts::AdapterMode::Live);
            (!any_live, simulated)
        })
        .unwrap_or((false, Vec::new()));
    let integrations: Vec<shell::StatusSeg> = status
        .data
        .as_ref()
        .map(|status| {
            status
                .integrations
                .iter()
                .map(|integration| {
                    let (mode_label, mode_tone) = labels::adapter_mode(integration.mode);
                    // Fixture is always "fixture tested"; only live needs its evidence level.
                    let detail = match integration.mode {
                        contracts::AdapterMode::Live => labels::verification(integration.verification).0,
                        _ => mode_label,
                    };
                    shell::StatusSeg {
                        label: format!("{} · {}", integration.name, detail),
                        tone: if mode_tone == "ok" { "live".to_string() } else { mode_tone.to_string() },
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let production_paused = status.data.as_ref().is_some_and(|status| status.production_paused);
    // Fleet is infrastructure: it leads the bar, and a stale snapshot never
    // reads as OK.
    let fleet_loaded = (store.fleet).cloned();
    let fleet_segment = match fleet_loaded.data.as_ref() {
        // A failed refresh outranks the cached snapshot it leaves behind.
        _ if fleet_loaded.error.is_some() => shell::StatusSeg { label: "Fleet unavailable".into(), tone: "bad".into() },
        None => shell::StatusSeg { label: "Loading fleet…".into(), tone: String::new() },
        Some(snapshot) if fleet::stale(format::now_ms(), snapshot) => {
            shell::StatusSeg { label: "Fleet snapshot stale".into(), tone: "warn".into() }
        }
        Some(snapshot) => {
            // The rows the console lists, so the bar and the list agree.
            let attention = fleet::attention_rows(snapshot).len();
            shell::StatusSeg {
                label: if attention == 0 { "Fleet OK".into() } else { format!("Fleet · {attention} need attention") },
                tone: if attention == 0 { "live".into() } else { "warn".into() },
            }
        }
    };
    let api_version = status.data.as_ref().map(|status| status.api_version.clone()).unwrap_or_else(|| "v1".into());
    let (workspace, inspector_item) = inspector_selection.cloned();
    let inspector_key = format!("{workspace:?}:{inspector_item:?}");
    let inspector_available = workspace.supports_inspector();
    let inspector_open = inspector_available && (store.inspector_open).cloned();
    let stage_class = format!(
        "stage{}{}",
        if inspector_open { "" } else { " inspector-closed" },
        if workspace.full_width() { " no-context" } else { "" }
    );

    let inspector_data: Option<inspector::InspectorData> = if !inspector_open {
        None
    } else {
        match workspace {
            Workspace::Studio => (store.content_detail)
                .cloned()
                .data
                .as_ref()
                .filter(|detail| Some(detail.item.id) == (store.selected_content).cloned())
                .map(inspector::from_content),
            Workspace::Relationships => (store.relationship_detail)
                .cloned()
                .data
                .as_ref()
                .filter(|detail| Some(detail.relationship.id) == (store.selected_relationship).cloned())
                .map(inspector::from_relationship),
            Workspace::Projects if (store.projects_show_individual).cloned() => {
                let canon = (store.canon).cloned();
                (store.personas)
                    .cloned()
                    .data
                    .as_ref()
                    .and_then(|personas| personas.iter().find(|persona| Some(persona.id) == (store.selected_persona).cloned()))
                    .map(|persona| inspector::from_persona(persona, canon.data.as_deref().unwrap_or(&[])))
            }
            Workspace::News => (store.news)
                .cloned()
                .data
                .as_ref()
                .and_then(|items| items.iter().find(|item| Some(item.id) == (store.selected_news).cloned()))
                .map(inspector::from_news),
            Workspace::Hosts => {
                let loaded = (store.fleet).cloned();
                let selected = (store.selected_host).cloned();
                loaded.data.as_ref().and_then(|snapshot| {
                    snapshot
                        .hosts
                        .iter()
                        .find(|host| Some(&host.name) == selected.as_ref())
                        .map(|host| inspector::from_host(host, snapshot.target_revision.as_deref()))
                })
            }
            _ => None,
        }
    };

    let action_client = client.clone();
    let on_action = move |action: actions::Action| {
        store.set_palette_open(false);
        match action {
            actions::Action::Workspace(workspace) => store.set_workspace(workspace),
            actions::Action::ToggleInspector => store.toggle_inspector(),
            actions::Action::Refresh => {
                let client = action_client.clone();
                spawn(async move {
                    api::refresh_base(store, client.clone()).await;
                    api::refresh_selected(store, client).await;
                });
            }
            actions::Action::RefreshFleet => {
                let client = action_client.clone();
                spawn(async move { api::refresh_fleet(store, client).await });
            }
            actions::Action::OpenSubject(subject_type, id) => actions::open_subject(store, &subject_type, id),
        }
    };

    rsx! {
        document::Title { "fleetix hub" }
        style { dangerous_inner_html: theme::style_sheet() }
        div { class: "app",
            if operations {
                FleetPoll {}
            }
            shell::Sidebar { workspace: store.workspace, nav_open: store.nav_open, narrow: narrow_nav }
            div { class: "main",
                shell::Topbar {
                    workspace,
                    inspector_open: store.inspector_open,
                    nav_open: store.nav_open,
                    on_palette: move |_| store.set_palette_open(true),
                }
                if fixture {
                    shell::FixtureBanner { all_fixture, simulated: simulated_integrations }
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
                div { class: "{stage_class}",
                    match workspace {
                        Workspace::Notifications => rsx! { workspaces::notifications::NotificationsView {} },
                        Workspace::Command => rsx! { workspaces::fleet::CommandView {} },
                        Workspace::Hosts => rsx! { workspaces::fleet::HostsView {} },
                        Workspace::Alerts => rsx! { workspaces::fleet::AlertsView {} },
                        Workspace::Approvals => rsx! { workspaces::fleet::ApprovalsView {} },
                        Workspace::Providers => rsx! { workspaces::providers::ProvidersView {} },
                        Workspace::Today => rsx! { workspaces::today::TodayView {} },
                        Workspace::News => rsx! { workspaces::news::NewsView {} },
                        Workspace::Intelligence => rsx! { workspaces::intelligence::IntelligenceView {} },
                        Workspace::Studio => rsx! { workspaces::studio::StudioView {} },
                        Workspace::Calendar => rsx! { workspaces::calendar::CalendarView {} },
                        Workspace::Mail => rsx! { workspaces::mail::MailView {} },
                        Workspace::Relationships => rsx! { workspaces::relationships::RelationshipsView {} },
                        Workspace::Projects => rsx! { workspaces::projects::ProjectsView {} },
                        Workspace::Library => rsx! { workspaces::library::LibraryView {} },
                    }
                    if inspector_open {
                        inspector::Inspector {
                            key: "{inspector_key}",
                            data: inspector_data,
                            has_selection: inspector_item.is_some(),
                            on_close: move |_| store.set_inspector_open(false),
                        }
                    }
                }
            }
            shell::StatusBar {
                connection: shell::StatusSeg {
                    label: status_label,
                    tone: if live { "live" } else if status.error.is_some() { "bad" } else { "" }.into(),
                },
                fleet: operations.then_some(fleet_segment),
                production_paused,
                integrations,
                version: format!("API {api_version} · Fleetix Hub v{}", env!("CARGO_PKG_VERSION")),
            }
            if (store.palette_open).cloned() {
                palette::CommandPalette {
                    inspector_available,
                    on_action,
                    on_close: move |_| store.set_palette_open(false),
                }
            }
        }
    }
}

// Mount only after status authorizes fleet access; refresh immediately, then every 30s.
#[component]
fn FleetPoll() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<contracts::client::HubClient>();
    use_future(move || {
        let client = client.clone();
        async move {
            loop {
                api::refresh_fleet(store, client.clone()).await;
                tokio::time::sleep(Duration::from_secs(api::FLEET_POLL_SECS)).await;
            }
        }
    });
    VNode::empty()
}
