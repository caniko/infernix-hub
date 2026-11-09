//! infernix-hub desktop — local-first content operations console.
//!
//! Milestone A: the donor's Dioxus app shell (sidebar / topbar / status bar,
//! stat cards, history feed, chat transcript, approval gate, proposal review,
//! delta rows, list editor) re-hosted as a Dioxus 0.6 **desktop** app with the
//! hub palette and static placeholder data. Milestone C replaces the demo data
//! with `hub_contracts::client::HubClient`.
//!
//! Donor: aivyx-pa `crates/aivyx-web` (MIT). The Aivyx/Stitch branding, logos,
//! WebSocket/daemon coupling and persona/mission views are intentionally not
//! carried over; see `docs/provenance-desktop.md`.

mod components;
mod format;
mod icons;
mod inspector;
mod shell;
mod theme;
mod workspaces;

use dioxus::prelude::*;

/// The six operator workspaces. The sidebar order is the keyboard order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    Today,
    News,
    Studio,
    Calendar,
    Relationships,
    Library,
}

impl Workspace {
    pub const ALL: [Workspace; 6] = [
        Workspace::Today,
        Workspace::News,
        Workspace::Studio,
        Workspace::Calendar,
        Workspace::Relationships,
        Workspace::Library,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Workspace::Today => "Today",
            Workspace::News => "News",
            Workspace::Studio => "Studio",
            Workspace::Calendar => "Calendar",
            Workspace::Relationships => "Relationships",
            Workspace::Library => "Library",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Workspace::Today => "Today",
            Workspace::News => "News",
            Workspace::Studio => "Studio",
            Workspace::Calendar => "Calendar",
            Workspace::Relationships => "Relationships",
            Workspace::Library => "Library",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Workspace::Today => "reviews, production, due releases, failed work, overdue replies",
            Workspace::News => "coverage, evidence and brief creation",
            Workspace::Studio => "brief, generation, assets, release freeze and scheduling",
            Workspace::Calendar => "intents by day and account",
            Workspace::Relationships => "threads, ownership, drafts and handoff",
            Workspace::Library => "personas, canon review, assets",
        }
    }
}

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let workspace = use_signal(|| Workspace::Today);
    let mut inspector_open = use_signal(|| true);
    let nav_open = use_signal(|| false);

    let demo_inspector = inspector::InspectorData {
        title: "Shell preview".into(),
        ids: vec![
            ("api".into(), "/v1".into()),
            ("schema".into(), "hub-contracts".into()),
            ("workspace".into(), workspace().label().into()),
        ],
        foreign: vec![("note".into(), "no live selection".into())],
        payload_hash: None,
        raw: "{\n  \"milestone\": \"A\",\n  \"data\": \"static placeholder\"\n}".into(),
        history: vec![],
    };

    rsx! {
        document::Title { "infernix hub" }
        style { dangerous_inner_html: theme::style_sheet() }
        div { class: "app",
            shell::Sidebar { workspace, nav_open }
            div { class: "main",
                shell::Topbar {
                    workspace: workspace(),
                    inspector_open,
                    nav_open,
                    live: false,
                    status_label: "shell preview".to_string(),
                }
                div { class: if inspector_open() { "stage" } else { "stage inspector-closed" },
                    match workspace() {
                        Workspace::Today => rsx! { workspaces::TodayView {} },
                        Workspace::News => rsx! { workspaces::NewsView {} },
                        Workspace::Studio => rsx! { workspaces::StudioView {} },
                        Workspace::Calendar => rsx! { workspaces::CalendarView {} },
                        Workspace::Relationships => rsx! { workspaces::RelationshipsView {} },
                        Workspace::Library => rsx! { workspaces::LibraryView {} },
                    }
                    if inspector_open() {
                        inspector::Inspector {
                            data: Some(demo_inspector),
                            on_close: move |_| inspector_open.set(false),
                        }
                    }
                }
            }
            shell::StatusBar {
                connected: false,
                items: vec![
                    shell::StatusSeg { label: "FIXTURE · NOT CONNECTED".into(), tone: "warn".into() },
                    shell::StatusSeg { label: "DIOXUS 0.6".into(), tone: "".into() },
                ],
                right: "INFERNIX-HUB v0.1.0".to_string(),
            }
        }
    }
}
