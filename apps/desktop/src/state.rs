//! Shared UI state. Every remote read is a `Loaded<T>` so a failed refresh can
//! keep the last good data while the error is shown; every list has an explicit
//! empty state.

use std::collections::{HashMap, HashSet};

use contracts::{
    Account, Asset, CanonRevision, ChatMessage, ContentDetail, ContentItem, NewsItem, Notification, NotificationPage, Persona, Project,
    PublicationIntent, Relationship, RelationshipDetail, ReleaseState, Renewal, SystemStatus, Today, Workflow,
};
use dioxus::prelude::*;
use uuid::Uuid;

/// Operator workspaces. The app opens on Command; slots are independent of sidebar position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workspace {
    Notifications,
    Command,
    Hosts,
    Alerts,
    Approvals,
    Providers,
    Today,
    News,
    Studio,
    Calendar,
    Relationships,
    Projects,
    Library,
}

impl Workspace {
    pub const ALL: [Workspace; 13] = [
        Workspace::Notifications,
        Workspace::Command,
        Workspace::Hosts,
        Workspace::Alerts,
        Workspace::Approvals,
        Workspace::Providers,
        Workspace::Today,
        Workspace::News,
        Workspace::Studio,
        Workspace::Calendar,
        Workspace::Relationships,
        Workspace::Projects,
        Workspace::Library,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Workspace::Notifications => "Notifications",
            Workspace::Command => "Command Center",
            Workspace::Hosts => "Hosts",
            Workspace::Alerts => "Alerts",
            Workspace::Approvals => "Approvals",
            Workspace::Providers => "AI Usage",
            Workspace::Today => "Today",
            Workspace::News => "News",
            Workspace::Studio => "Studio",
            Workspace::Calendar => "Calendar",
            Workspace::Relationships => "Relationships",
            Workspace::Projects => "Projects",
            Workspace::Library => "Library",
        }
    }

    /// `--workspace NAME` value; the tray starts the console with
    /// `--workspace notifications`.
    pub fn slug(self) -> &'static str {
        match self {
            Workspace::Notifications => "notifications",
            Workspace::Command => "command",
            Workspace::Hosts => "hosts",
            Workspace::Alerts => "alerts",
            Workspace::Approvals => "approvals",
            Workspace::Providers => "providers",
            Workspace::Today => "today",
            Workspace::News => "news",
            Workspace::Studio => "studio",
            Workspace::Calendar => "calendar",
            Workspace::Relationships => "relationships",
            Workspace::Projects => "projects",
            Workspace::Library => "library",
        }
    }

    pub fn from_name(name: &str) -> Option<Workspace> {
        let name = name.trim().to_ascii_lowercase();
        Self::ALL.iter().copied().find(|workspace| workspace.slug() == name)
    }

    /// The sidebar's keyboard shortcut.
    pub fn key(self) -> &'static str {
        match self {
            Workspace::Notifications => "n",
            Workspace::Command => "1",
            Workspace::Hosts => "2",
            Workspace::Alerts => "3",
            Workspace::Approvals => "4",
            Workspace::Providers => "u",
            Workspace::Today => "5",
            Workspace::News => "6",
            Workspace::Studio => "7",
            Workspace::Calendar => "8",
            Workspace::Relationships => "9",
            Workspace::Projects => "p",
            Workspace::Library => "0",
        }
    }

    pub fn from_key(key: &str) -> Option<Workspace> {
        Self::ALL.iter().copied().find(|workspace| workspace.key() == key)
    }

    pub fn title(self) -> &'static str {
        self.label()
    }

    pub fn hint(self) -> &'static str {
        match self {
            Workspace::Notifications => "Conditions the hub wants you to see, newest first",
            Workspace::Command => "Fleet health, attention queue and snapshot age",
            Workspace::Hosts => "Health, revision and failed checks per host",
            Workspace::Alerts => "Monitored services that are down",
            Workspace::Approvals => "Privileged operations waiting for the operator",
            Workspace::Providers => "Model provider accounts, remaining quota, activity, routing and exports",
            Workspace::Today => "Reviews, production, due releases, failed work and overdue replies",
            Workspace::News => "Coverage, evidence and brief creation",
            Workspace::Studio => "Briefs, generation, assets, release freeze and scheduling",
            Workspace::Calendar => "Scheduled posts by day",
            Workspace::Relationships => "Threads, ownership, drafts and handoff",
            Workspace::Projects => "Influencer projects and individuals",
            Workspace::Library => "Assets",
        }
    }

    pub fn supports_inspector(self) -> bool {
        matches!(self, Self::Hosts | Self::News | Self::Studio | Self::Relationships | Self::Projects)
    }

    /// Sidebar section heading.
    pub fn section(self) -> &'static str {
        match self {
            Workspace::Notifications
            | Workspace::Command
            | Workspace::Hosts
            | Workspace::Alerts
            | Workspace::Approvals
            | Workspace::Providers => "FLEET",
            _ => "STUDIO",
        }
    }

    /// Fleet workspaces use the whole stage: their lists carry their own
    /// columns, so there is no separate context list to pin beside them.
    pub fn full_width(self) -> bool {
        // Library lost its context list when personas moved to Projects.
        self.section() == "FLEET" || self == Workspace::Library
    }
}

/// The start workspace from `--workspace NAME` (also `--workspace=NAME`).
/// Without the flag, or with a name that is not a workspace, the console opens
/// on Command; the unknown name is returned so the caller can report it.
pub fn start_workspace(args: &[String]) -> (Workspace, Option<String>) {
    let mut name = None;
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--workspace=") {
            name = Some(value.to_string());
        } else if arg == "--workspace" {
            name = args.next().cloned();
        }
    }
    match name {
        Some(name) => match Workspace::from_name(&name) {
            Some(workspace) => (workspace, None),
            None => (Workspace::Command, Some(name)),
        },
        None => (Workspace::Command, None),
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

/// The notification feed's client state.
///
/// `items` is keyed by id: the first page is a snapshot with `all`, and every
/// later page is the feed of changed rows, so it folds in instead of replacing
/// the rest. `open` and `critical` are the server's counts for *all* open rows
/// and therefore survive any local filter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Notifications {
    pub items: HashMap<Uuid, Notification>,
    pub open: i64,
    pub critical: i64,
    /// True once any page (even an empty one) has been applied.
    pub loaded: bool,
    pub feed_error: Option<UiError>,
}

/// All signals the console shares. `Signal` is `Copy`, so `Store` is too.
#[derive(Clone, Copy)]
pub struct Store {
    pub workspace: Signal<Workspace>,
    pub status: Signal<Loaded<SystemStatus>>,
    /// Fleet snapshot; refreshed on its own 30s cadence, not the 1.5s poll.
    pub fleet: Signal<Loaded<contracts::fleet::FleetStatus>>,
    /// The long-polled notification set; see [`Notifications`].
    pub notifications: Signal<Notifications>,
    /// Renewals, read on demand by the notification workspace's expansions.
    pub renewals: Signal<Loaded<Vec<Renewal>>>,
    pub today: Signal<Loaded<Today>>,
    pub news: Signal<Loaded<Vec<NewsItem>>>,
    pub personas: Signal<Loaded<Vec<Persona>>>,
    pub projects: Signal<Loaded<Vec<Project>>>,
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

    pub selected_content: Signal<Option<Uuid>>,
    pub selected_news: Signal<Option<Uuid>>,
    pub selected_persona: Signal<Option<Uuid>>,
    pub selected_project: Signal<Option<Uuid>>,
    pub projects_show_individual: Signal<bool>,
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
        if (self.workspace).cloned().supports_inspector() {
            self.set_inspector_open(!(self.inspector_open).cloned());
        }
    }

    /// Selection identity, not the polled payload: refreshes must never reopen
    /// an inspector the operator dismissed.
    pub fn inspector_item(&self, workspace: Workspace) -> Option<String> {
        let id = match workspace {
            Workspace::Hosts => return (self.selected_host).cloned(),
            Workspace::News => (self.selected_news).cloned(),
            Workspace::Studio => (self.selected_content).cloned(),
            Workspace::Relationships => (self.selected_relationship).cloned(),
            Workspace::Projects if (self.projects_show_individual).cloned() => (self.selected_persona).cloned(),
            _ => None,
        };
        id.map(|id| id.to_string())
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
    pub fn set_selected_content(&self, value: Option<Uuid>) {
        let mut signal = self.selected_content;
        signal.set(value);
    }
    pub fn set_selected_news(&self, value: Option<Uuid>) {
        let mut signal = self.selected_news;
        signal.set(value);
    }
    pub fn set_selected_persona(&self, value: Option<Uuid>) {
        let mut show_individual = self.projects_show_individual;
        show_individual.set(value.is_some());
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
    /// Replace the set: the snapshot carries every row, so absent rows are gone.
    pub fn set_notification_snapshot(&self, page: NotificationPage) {
        let mut signal = self.notifications;
        signal.set(Notifications {
            items: page.items.into_iter().map(|item| (item.id, item)).collect(),
            open: page.open,
            critical: page.critical,
            loaded: true,
            feed_error: None,
        });
    }
    /// Fold the change feed in; it only carries rows whose `seq` moved.
    pub fn merge_notification_page(&self, page: NotificationPage) {
        let mut signal = self.notifications;
        let mut current = signal.cloned();
        for item in page.items {
            current.items.insert(item.id, item);
        }
        current.open = page.open;
        current.critical = page.critical;
        current.loaded = true;
        current.feed_error = None;
        signal.set(current);
    }
    /// A failed feed keeps the rows on screen; the next page clears the error.
    pub fn fail_notifications(&self, error: UiError) {
        let mut signal = self.notifications;
        let mut current = signal.cloned();
        current.feed_error = Some(error);
        signal.set(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn start_workspace_reads_the_flag() {
        assert_eq!(start_workspace(&args(&["fleetix-hub-desktop"])), (Workspace::Command, None));
        assert_eq!(start_workspace(&args(&["app", "--workspace", "notifications"])), (Workspace::Notifications, None));
        assert_eq!(start_workspace(&args(&["app", "--workspace=today"])), (Workspace::Today, None));
        assert_eq!(start_workspace(&args(&["app", "--workspace", "TODAY"])), (Workspace::Today, None));
        // The last flag wins; an unknown name falls back and is reported.
        assert_eq!(
            start_workspace(&args(&["app", "--workspace", "news", "--workspace", "nope"])),
            (Workspace::Command, Some("nope".to_string()))
        );
        // A flag without a value is not a workspace request.
        assert_eq!(start_workspace(&args(&["app", "--workspace"])), (Workspace::Command, None));
    }

    #[test]
    fn every_workspace_has_a_unique_key_and_slug() {
        let mut keys: Vec<&str> = Workspace::ALL.iter().map(|workspace| workspace.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), Workspace::ALL.len(), "keyboard keys are unique");
        for workspace in Workspace::ALL {
            assert_eq!(Workspace::from_key(workspace.key()), Some(workspace));
            assert_eq!(Workspace::from_name(workspace.slug()), Some(workspace));
            assert!(!workspace.label().is_empty());
        }
        assert_eq!(Workspace::from_key("x"), None);
        assert_eq!(Workspace::from_name("nope"), None);
        assert_eq!(Workspace::Notifications.key(), "n", "Notifications keeps its letter");
        assert_eq!(Workspace::Command.key(), "1", "the ten original digits do not move");
        assert_eq!(Workspace::Library.key(), "0");
    }

    #[test]
    fn ai_usage_is_a_full_width_fleet_workspace() {
        let usage = Workspace::from_name("providers").expect("AI Usage workspace");
        assert_eq!(Workspace::from_key("u"), Some(usage));
        assert_eq!(usage.label(), "AI Usage");
        assert_eq!(usage.section(), "FLEET");
        assert!(usage.full_width());
    }

    #[test]
    fn projects_sits_in_the_studio_section() {
        assert_eq!(Workspace::Projects.section(), "STUDIO");
        let position = Workspace::ALL.iter().position(|w| *w == Workspace::Projects).unwrap();
        assert_eq!(Workspace::ALL[position + 1], Workspace::Library);
        assert_eq!(start_workspace(&args(&["app", "--workspace", "projects"])), (Workspace::Projects, None));
    }
}
