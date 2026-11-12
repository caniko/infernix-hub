//! Shared UI state. Every remote read is a `Loaded<T>` so a failed refresh can
//! keep the last good data while the error is shown; every list has an explicit
//! empty state.

use std::collections::{HashMap, HashSet};

use contracts::{
    Account, Asset, CanonRevision, ChatMessage, ContentDetail, ContentItem, NewsItem, Persona, PublicationIntent, Relationship,
    RelationshipDetail, ReleaseState, SearchHit, SystemStatus, Today, Workflow,
};
use dioxus::prelude::*;
use uuid::Uuid;

/// The ten operator workspaces. Fleet leads (the app opens on Command), then
/// Studio; sidebar order is the 1–9/0 keyboard order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    Command,
    Hosts,
    Alerts,
    Approvals,
    Today,
    News,
    Studio,
    Calendar,
    Relationships,
    Library,
}

impl Workspace {
    pub const ALL: [Workspace; 10] = [
        Workspace::Command,
        Workspace::Hosts,
        Workspace::Alerts,
        Workspace::Approvals,
        Workspace::Today,
        Workspace::News,
        Workspace::Studio,
        Workspace::Calendar,
        Workspace::Relationships,
        Workspace::Library,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Workspace::Command => "Command Center",
            Workspace::Hosts => "Hosts",
            Workspace::Alerts => "Alerts",
            Workspace::Approvals => "Approvals",
            Workspace::Today => "Today",
            Workspace::News => "News",
            Workspace::Studio => "Studio",
            Workspace::Calendar => "Calendar",
            Workspace::Relationships => "Relationships",
            Workspace::Library => "Library",
        }
    }

    pub fn title(self) -> &'static str {
        self.label()
    }

    pub fn hint(self) -> &'static str {
        match self {
            Workspace::Command => "fleet health, attention queue and snapshot age",
            Workspace::Hosts => "health, revision and failed checks per host",
            Workspace::Alerts => "monitored services that are down",
            Workspace::Approvals => "privileged operations waiting for the operator",
            Workspace::Today => "reviews, production, due releases, failed work, overdue replies",
            Workspace::News => "coverage, evidence and brief creation",
            Workspace::Studio => "brief, generation, assets, release freeze and scheduling",
            Workspace::Calendar => "intents by day and account",
            Workspace::Relationships => "threads, ownership, drafts and handoff",
            Workspace::Library => "personas, canon review, assets",
        }
    }

    /// Sidebar section heading.
    pub fn section(self) -> &'static str {
        match self {
            Workspace::Command | Workspace::Hosts | Workspace::Alerts | Workspace::Approvals => "FLEET",
            _ => "STUDIO",
        }
    }

    /// Fleet workspaces use the whole stage: their lists carry their own
    /// columns, so there is no separate context list to pin beside them.
    pub fn full_width(self) -> bool {
        self.section() == "FLEET"
    }
}

/// An error shaped for the UI: stable code plus a human message.
#[derive(Debug, Clone, PartialEq)]
pub struct UiError {
    pub code: String,
    pub message: String,
}

impl UiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }

    pub fn from_client(error: contracts::client::ClientError) -> Self {
        match error {
            contracts::client::ClientError::Api { status, error } => {
                Self { code: error.code, message: format!("{} (HTTP {status})", error.message) }
            }
            contracts::client::ClientError::Transport(message) => Self { code: "transport".into(), message },
        }
    }
}

/// A remote read. `data` survives refresh failures; `error` is cleared by the
/// next success. `loading` starts true so first paint shows a loading state.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded<T> {
    pub data: Option<T>,
    pub error: Option<UiError>,
    pub loading: bool,
}

impl<T> Default for Loaded<T> {
    fn default() -> Self {
        Self { data: None, error: None, loading: true }
    }
}

impl<T> Loaded<T> {
    pub fn ready(data: T) -> Self {
        Self { data: Some(data), error: None, loading: false }
    }

    /// Keep the last good data, surface the new error.
    pub fn failed(&mut self, error: UiError) {
        self.error = Some(error);
        self.loading = false;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub ok: bool,
    pub text: String,
}

impl Notice {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { ok: true, text: text.into() }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self { ok: false, text: text.into() }
    }
}

/// All signals the console shares. `Signal` is `Copy`, so `Store` is too.
#[derive(Clone, Copy)]
pub struct Store {
    pub workspace: Signal<Workspace>,
    pub status: Signal<Loaded<SystemStatus>>,
    /// Fleet snapshot; refreshed on its own 30s cadence, not the 1.5s poll.
    pub fleet: Signal<Loaded<contracts::fleet::FleetStatus>>,
    pub today: Signal<Loaded<Today>>,
    pub news: Signal<Loaded<Vec<NewsItem>>>,
    pub personas: Signal<Loaded<Vec<Persona>>>,
    pub workflows: Signal<Loaded<Vec<Workflow>>>,
    pub accounts: Signal<Loaded<Vec<Account>>>,
    pub content: Signal<Loaded<Vec<ContentItem>>>,
    pub intents: Signal<Loaded<Vec<PublicationIntent>>>,
    pub relationships: Signal<Loaded<Vec<Relationship>>>,
    pub assets: Signal<Loaded<Vec<Asset>>>,
    pub content_detail: Signal<Loaded<ContentDetail>>,
    pub relationship_detail: Signal<Loaded<RelationshipDetail>>,
    pub canon: Signal<Loaded<Vec<CanonRevision>>>,
    pub chat_content: Signal<Loaded<Vec<ChatMessage>>>,
    pub chat_persona: Signal<Loaded<Vec<ChatMessage>>>,
    pub search: Signal<Loaded<Vec<SearchHit>>>,

    pub selected_content: Signal<Option<Uuid>>,
    pub selected_news: Signal<Option<Uuid>>,
    pub selected_persona: Signal<Option<Uuid>>,
    pub selected_relationship: Signal<Option<Uuid>>,
    /// Hosts are identified by name in the snapshot, not by an id.
    pub selected_host: Signal<Option<String>>,
    /// Set by a chat proposal link so Library highlights the matching card.
    pub focus_proposal: Signal<Option<Uuid>>,

    /// Asset id → `data:` URI (image or video), loaded from `/v1/assets/{id}/content`.
    pub asset_images: Signal<HashMap<Uuid, String>>,
    /// Release revision id → approval state, built from content details.
    pub release_states: Signal<HashMap<Uuid, ReleaseState>>,
    /// Release revision id → content item id.
    pub release_content: Signal<HashMap<Uuid, Uuid>>,
    /// Content items whose release list has been indexed.
    pub release_indexed: Signal<HashSet<Uuid>>,

    pub notice: Signal<Option<Notice>>,
    pub inspector_open: Signal<bool>,
    pub nav_open: Signal<bool>,
    pub palette_open: Signal<bool>,
    pub poll_busy: Signal<bool>,
}

impl Store {
    pub fn notify(&self, ok: bool, text: impl Into<String>) {
        let text = text.into();
        let mut notice = self.notice;
        notice.set(Some(if ok { Notice::ok(text) } else { Notice::err(text) }));
    }

    pub fn clear_notice(&self) {
        let mut notice = self.notice;
        notice.set(None);
    }

    pub fn toggle_inspector(&self) {
        let mut open = self.inspector_open;
        open.set(!open.cloned());
    }

    pub fn insert_asset_image(&self, id: Uuid, uri: String) {
        let mut images = self.asset_images;
        images.write().insert(id, uri);
    }

    pub fn release_state(&self, release: Uuid) -> Option<ReleaseState> {
        (self.release_states).cloned().get(&release).copied()
    }

    pub fn release_content_id(&self, release: Uuid) -> Option<Uuid> {
        (self.release_content).cloned().get(&release).copied()
    }

    pub fn is_release_indexed(&self, content: Uuid) -> bool {
        (self.release_indexed).cloned().contains(&content)
    }

    pub fn index_release(&self, release: Uuid, content: Uuid, state: ReleaseState) {
        let mut states = self.release_states;
        states.write().insert(release, state);
        let mut links = self.release_content;
        links.write().insert(release, content);
    }

    pub fn mark_release_indexed(&self, content: Uuid) {
        let mut indexed = self.release_indexed;
        indexed.write().insert(content);
    }

    pub fn clear_release_index(&self) {
        let mut states = self.release_states;
        states.set(HashMap::new());
        let mut links = self.release_content;
        links.set(HashMap::new());
        let mut indexed = self.release_indexed;
        indexed.set(HashSet::new());
    }
}

/// Signals are `Copy` but `set`/`write` need a mutable place; these setters keep
/// the shared `Store` usable from `&self` async handlers.
impl Store {
    pub fn set_workspace(&self, value: Workspace) {
        let mut signal = self.workspace;
        signal.set(value);
    }
    pub fn set_content_detail(&self, value: Loaded<ContentDetail>) {
        let mut signal = self.content_detail;
        signal.set(value);
    }
    pub fn set_relationship_detail(&self, value: Loaded<RelationshipDetail>) {
        let mut signal = self.relationship_detail;
        signal.set(value);
    }
    pub fn set_chat_content(&self, value: Loaded<Vec<ChatMessage>>) {
        let mut signal = self.chat_content;
        signal.set(value);
    }
    pub fn set_chat_persona(&self, value: Loaded<Vec<ChatMessage>>) {
        let mut signal = self.chat_persona;
        signal.set(value);
    }
    pub fn set_search(&self, value: Loaded<Vec<SearchHit>>) {
        let mut signal = self.search;
        signal.set(value);
    }
    pub fn set_selected_content(&self, value: Option<Uuid>) {
        let mut signal = self.selected_content;
        signal.set(value);
    }
    pub fn set_selected_news(&self, value: Option<Uuid>) {
        let mut signal = self.selected_news;
        signal.set(value);
    }
    pub fn set_selected_persona(&self, value: Option<Uuid>) {
        let mut signal = self.selected_persona;
        if signal.cloned() != value {
            // Canon is loaded per persona; never let the next persona read the previous one's.
            let mut canon = self.canon;
            canon.set(Loaded::default());
        }
        signal.set(value);
    }
    pub fn set_selected_relationship(&self, value: Option<Uuid>) {
        let mut signal = self.selected_relationship;
        signal.set(value);
    }
    pub fn set_selected_host(&self, value: Option<String>) {
        let mut signal = self.selected_host;
        signal.set(value);
    }
    pub fn set_focus_proposal(&self, value: Option<Uuid>) {
        let mut signal = self.focus_proposal;
        signal.set(value);
    }
    pub fn set_inspector_open(&self, value: bool) {
        let mut signal = self.inspector_open;
        signal.set(value);
    }
    pub fn set_nav_open(&self, value: bool) {
        let mut signal = self.nav_open;
        signal.set(value);
    }
    pub fn set_palette_open(&self, value: bool) {
        let mut signal = self.palette_open;
        signal.set(value);
    }
    pub fn set_poll_busy(&self, value: bool) {
        let mut signal = self.poll_busy;
        signal.set(value);
    }
}
