//! `fleetix-hub-tray`: the desktop side of hub notifications.
//!
//! One `current_thread` runtime, one long-poll connection, a StatusNotifierItem
//! icon and freedesktop popups. No GTK or WebKit is linked: the console is
//! spawned on demand and exits on close (plan D1). The 2026-11-15 COSMIC spike
//! proved the ksni item, its menu, `Activate` and `Notify`; `ActionInvoked`
//! needs a real click, so popups carry no actions and the icon, tooltip and
//! popups alone carry the state.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use contracts::client::HubClient;
use contracts::{AckNotificationsIn, Notification, NotificationPage, Severity};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{ToolTip, Tray, TrayMethods};
use tokio::sync::mpsc;
use zbus::zvariant::Value;

/// The server holds a feed request for this long; the client waits longer.
const FEED_WAIT: u64 = 50;
const FETCH_TIMEOUT: Duration = Duration::from_secs(70);
const BACKOFF_START: u64 = 5;
const BACKOFF_MAX: u64 = 60;
/// Newest open rows kept for the menu.
const MENU_ROWS: usize = 5;
/// More new rows than this arrive as one summary popup.
const POPUP_LIMIT: usize = 3;

#[derive(Default)]
struct State {
    offline: bool,
    open: i64,
    critical: i64,
    /// `None` until the first snapshot; a feed cursor afterwards.
    cursor: Option<i64>,
    /// Newest open rows, at most [`MENU_ROWS`], keyed by id.
    items: Vec<Notification>,
}

#[derive(Debug, PartialEq)]
struct Popup {
    title: String,
    body: String,
    critical: bool,
}

#[derive(Debug, PartialEq)]
struct View {
    icon: &'static str,
    status: ksni::Status,
    tooltip: String,
    /// Newest open titles; the tray adds its fixed entries around them.
    menu: Vec<String>,
}

/// Fold one page into the state and return the popups it announces.
///
/// A feed only carries rows whose `seq` moved, so every `open` row in it is new
/// or reopened. The startup snapshot is the same call: the caller drops these
/// popups and announces the backlog as one line instead.
fn apply(state: &mut State, page: NotificationPage) -> Vec<Popup> {
    state.cursor = Some(page.cursor);
    state.open = page.open;
    state.critical = page.critical;
    for item in &page.items {
        match state.items.iter().position(|known| known.id == item.id) {
            Some(at) if item.open => state.items[at] = item.clone(),
            Some(at) => {
                state.items.remove(at);
            }
            None if item.open => state.items.push(item.clone()),
            None => {}
        }
    }
    state.items.sort_by_key(|item| std::cmp::Reverse(item.seq));
    state.items.truncate(MENU_ROWS);

    let popups: Vec<Popup> = page
        .items
        .iter()
        .filter(|item| item.open)
        .map(|item| Popup { title: item.title.clone(), body: item.body.clone(), critical: item.severity == Severity::Critical })
        .collect();
    if popups.len() > POPUP_LIMIT {
        return vec![Popup {
            title: "Fleetix Hub".into(),
            body: format!("{} new hub notifications", popups.len()),
            critical: popups.iter().any(|popup| popup.critical),
        }];
    }
    popups
}

fn view(state: &State) -> View {
    let icon = if state.offline {
        "network-offline-symbolic"
    } else if state.critical > 0 {
        "dialog-warning-symbolic"
    } else if state.open > 0 {
        "mail-unread-symbolic"
    } else {
        "mail-read-symbolic"
    };
    View {
        icon,
        status: if state.open > 0 { ksni::Status::NeedsAttention } else { ksni::Status::Active },
        tooltip: format!("Fleetix Hub: {} open", state.open),
        menu: state.items.iter().map(|item| item.title.clone()).collect(),
    }
}

enum Action {
    Open,
    AckAll,
    Quit,
}

struct HubTray {
    view: View,
    actions: mpsc::UnboundedSender<Action>,
}

fn entry(label: impl Into<String>, activate: impl Fn(&mut HubTray) + Send + Sync + 'static) -> MenuItem<HubTray> {
    StandardItem { label: label.into(), activate: Box::new(activate), ..Default::default() }.into()
}

impl Tray for HubTray {
    fn id(&self) -> String {
        "fleetix-hub-tray".into()
    }
    fn title(&self) -> String {
        "Fleetix Hub".into()
    }
    fn icon_name(&self) -> String {
        self.view.icon.into()
    }
    fn status(&self) -> ksni::Status {
        self.view.status
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: self.view.tooltip.clone(), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::Open);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu = vec![entry("Open Fleetix Hub", |tray| {
            let _ = tray.actions.send(Action::Open);
        })];
        menu.extend(self.view.menu.iter().map(|title| {
            entry(title.clone(), |tray| {
                let _ = tray.actions.send(Action::Open);
            })
        }));
        menu.push(MenuItem::Separator);
        menu.push(entry("Acknowledge all", |tray| {
            let _ = tray.actions.send(Action::AckAll);
        }));
        menu.push(entry("Quit", |tray| {
            let _ = tray.actions.send(Action::Quit);
        }));
        menu
    }
}

struct Notifier {
    proxy: NotificationsProxy<'static>,
}

impl Notifier {
    /// `None` without a session bus or a notification server: popups are
    /// best-effort, the icon still carries the state.
    async fn connect() -> Option<Self> {
        let connection = zbus::Connection::session().await.ok()?;
        NotificationsProxy::new(&connection).await.ok().map(|proxy| Self { proxy })
    }

    async fn popup(&self, popup: &Popup) {
        let mut hints: HashMap<String, Value<'static>> = HashMap::new();
        hints.insert("urgency".into(), Value::U8(if popup.critical { 2 } else { 1 }));
        let _ = self.proxy.notify("Fleetix Hub", 0, "", &popup.title, &popup.body, &[], hints, -1).await;
    }
}

// The freedesktop `Notify` signature is the specification itself: eight
// arguments that cannot be shortened, so the lint is silenced for this module.
#[allow(clippy::too_many_arguments)]
mod freedesktop {
    use super::*;

    #[zbus::proxy(
        interface = "org.freedesktop.Notifications",
        default_service = "org.freedesktop.Notifications",
        default_path = "/org/freedesktop/Notifications"
    )]
    pub trait Notifications {
        fn notify(
            &self,
            app_name: &str,
            replaces_id: u32,
            app_icon: &str,
            summary: &str,
            body: &str,
            actions: &[&str],
            hints: HashMap<String, Value<'static>>,
            expire_timeout: i32,
        ) -> zbus::Result<u32>;
    }
}

use freedesktop::NotificationsProxy;

struct Desktop {
    command: String,
    child: Option<tokio::process::Child>,
}

impl Desktop {
    fn open(&mut self) {
        if let Some(child) = self.child.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            return;
        }
        // ponytail: no focus-existing on Wayland; one child at a time
        match tokio::process::Command::new(&self.command).arg("--workspace").arg("notifications").spawn() {
            Ok(child) => self.child = Some(child),
            Err(error) => eprintln!("fleetix-hub-tray: cannot start {}: {error}", self.command),
        }
    }
}

async fn fetch(client: &HubClient, after: Option<i64>, wait: u64) -> Result<NotificationPage, String> {
    match tokio::time::timeout(FETCH_TIMEOUT, client.notifications(after, wait, false)).await {
        Ok(Ok(page)) => Ok(page),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!("no answer within {} s", FETCH_TIMEOUT.as_secs())),
    }
}

/// Handle one tray click; `true` means the daemon should exit.
async fn act(action: Action, client: &HubClient, desktop: &mut Desktop) -> bool {
    match action {
        Action::Open => desktop.open(),
        // Nothing is applied here: the acknowledgement bumps `seq`, so the feed
        // reports the rows as closed and the in-flight poll wakes at once.
        Action::AckAll => {
            if let Err(error) = client.ack_notifications(&AckNotificationsIn { ids: Vec::new(), all: true }).await {
                eprintln!("fleetix-hub-tray: acknowledge all: {error}");
            }
        }
        Action::Quit => return true,
    }
    false
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let client = HubClient::new(
        std::env::var("HUB_URL").context("set HUB_URL, e.g. http://127.0.0.1:8470")?,
        std::env::var("HUB_TOKEN").context("set HUB_TOKEN to a hub operator token")?,
    );
    let desktop_command = std::env::var("HUB_DESKTOP_COMMAND").unwrap_or_else(|_| "fleetix-hub-desktop".into());

    let (sender, mut actions) = mpsc::unbounded_channel();
    let tray = HubTray { view: view(&State::default()), actions: sender };
    // COSMIC's status-area applet owns org.kde.StatusNotifierWatcher only while
    // it runs and the name is not D-Bus activatable, so a plain `spawn()` fails
    // whenever the tray starts before the shell (spike, 2026-11-15).
    let handle = tray.assume_sni_available(true).spawn().await.context("registering the status notifier item")?;
    let notifier = Notifier::connect().await;
    let mut desktop = Desktop { command: desktop_command, child: None };
    let mut state = State::default();
    let mut backoff = BACKOFF_START;

    loop {
        let snapshot = state.cursor.is_none();
        let wait = if snapshot { 0 } else { FEED_WAIT };
        let fetched = tokio::select! {
            page = fetch(&client, state.cursor, wait) => page,
            Some(action) = actions.recv() => {
                if act(action, &client, &mut desktop).await {
                    break;
                }
                continue;
            }
        };
        let page = match fetched {
            Ok(page) => page,
            Err(error) => {
                // Offline: no popup, and recovery takes a fresh snapshot so no
                // backlog is announced.
                state.offline = true;
                state.cursor = None;
                let _ = handle.update(|tray| tray.view = view(&state)).await;
                eprintln!("fleetix-hub-tray: {error}; retrying in {backoff}s");
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                    Some(action) = actions.recv() => if act(action, &client, &mut desktop).await {
                        break;
                    },
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        backoff = BACKOFF_START;
        state.offline = false;
        let popups = apply(&mut state, page);
        if let Some(notifier) = &notifier {
            if snapshot {
                if state.open > 0 {
                    let summary = Popup {
                        title: "Fleetix Hub".into(),
                        body: format!("{} open hub notifications", state.open),
                        critical: state.critical > 0,
                    };
                    notifier.popup(&summary).await;
                }
            } else {
                for popup in &popups {
                    notifier.popup(popup).await;
                }
            }
        }
        let _ = handle.update(|tray| tray.view = view(&state)).await;
    }

    handle.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn note(id: u128, seq: i64, title: &str, severity: Severity, open: bool) -> Notification {
        Notification {
            id: Uuid::from_u128(id),
            seq,
            source: "test".into(),
            dedupe_key: format!("test:{id}"),
            severity,
            title: title.into(),
            body: String::new(),
            subject_type: None,
            subject_id: None,
            url: None,
            read_at: None,
            resolved_at: None,
            snoozed_until: None,
            created_at: Default::default(),
            updated_at: Default::default(),
            open,
        }
    }

    fn page(cursor: i64, open: i64, critical: i64, items: Vec<Notification>) -> NotificationPage {
        NotificationPage { items, cursor, open, critical }
    }

    #[test]
    fn view_offline_shows_the_offline_icon() {
        // The icon reports reachability; the status still follows the last known
        // open count, so an unreachable hub cannot hide pending attention.
        let shown = view(&State { offline: true, open: 3, critical: 1, ..Default::default() });
        assert_eq!(shown.icon, "network-offline-symbolic");
        assert_eq!(shown.status, ksni::Status::NeedsAttention);
        assert_eq!(shown.tooltip, "Fleetix Hub: 3 open");
        assert_eq!(view(&State { offline: true, ..Default::default() }).status, ksni::Status::Active);
    }

    #[test]
    fn view_without_open_rows_is_quiet() {
        let view = view(&State::default());
        assert_eq!(view.icon, "mail-read-symbolic");
        assert_eq!(view.status, ksni::Status::Active);
        assert_eq!(view.tooltip, "Fleetix Hub: 0 open");
        assert!(view.menu.is_empty());
    }

    #[test]
    fn view_with_open_rows_wants_attention() {
        let state = State { open: 2, ..Default::default() };
        let view = view(&state);
        assert_eq!(view.icon, "mail-unread-symbolic");
        assert_eq!(view.status, ksni::Status::NeedsAttention);
        assert_eq!(view.tooltip, "Fleetix Hub: 2 open");
    }

    #[test]
    fn view_with_critical_rows_warns() {
        let state = State { open: 2, critical: 1, ..Default::default() };
        let view = view(&state);
        assert_eq!(view.icon, "dialog-warning-symbolic");
        assert_eq!(view.status, ksni::Status::NeedsAttention);
    }

    #[test]
    fn apply_pops_a_reopened_row_once_and_drops_it_from_the_menu_when_acked() {
        let mut state = State::default();
        let row = note(1, 5, "Renewal due", Severity::Warning, true);

        assert_eq!(apply(&mut state, page(5, 1, 0, vec![row.clone()])).len(), 1);
        assert_eq!(view(&state).menu, vec!["Renewal due".to_string()]);

        // Acknowledged: the feed returns the same row with `open == false`.
        let mut acked = row.clone();
        acked.open = false;
        acked.read_at = Some(Default::default());
        assert!(apply(&mut state, page(6, 0, 0, vec![acked])).is_empty());
        assert!(view(&state).menu.is_empty());
        assert_eq!(state.cursor, Some(6));

        // Reopened by its producer: exactly one popup, back in the menu.
        let mut reopened = row.clone();
        reopened.seq = 7;
        assert_eq!(apply(&mut state, page(7, 1, 0, vec![reopened])).len(), 1);
        assert_eq!(view(&state).menu, vec!["Renewal due".to_string()]);
    }

    #[test]
    fn apply_summarises_more_than_three_new_rows() {
        let mut state = State::default();
        let items = (0..4).map(|i| note(i, i as i64 + 1, &format!("row {i}"), Severity::Info, true)).collect();
        let popups = apply(&mut state, page(4, 4, 0, items));
        assert_eq!(popups.len(), 1);
        assert_eq!(popups[0].body, "4 new hub notifications");
        assert!(!popups[0].critical);
        assert_eq!(view(&state).menu.len(), 4);
    }

    #[test]
    fn apply_keeps_the_newest_five_titles_newest_first() {
        let mut state = State::default();
        let items = (0..7).map(|i| note(i, i as i64 + 1, &format!("row {i}"), Severity::Info, true)).collect();
        apply(&mut state, page(7, 7, 0, items));
        let menu = view(&state).menu;
        assert_eq!(menu.len(), MENU_ROWS);
        assert_eq!(menu[0], "row 6");
        assert_eq!(menu[4], "row 2");
    }
}
