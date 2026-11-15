//! `fleetix-hub-tray`: the desktop side of hub notifications.
//!
//! One `current_thread` runtime, one long-poll connection, a StatusNotifierItem
//! icon and freedesktop popups. No GTK or WebKit is linked: the console is
//! spawned on demand and exits on close (plan D1). The 2026-11-15 COSMIC spike
//! proved the ksni item, its menu, `Activate` and `Notify`; `ActionInvoked`
//! needs a real click, so popups carry no actions and the icon, tooltip and
//! popups alone carry the state.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use contracts::client::HubClient;
use contracts::{AckNotificationsIn, Notification, NotificationPage, Severity, SyncNotificationItem, SyncNotificationsIn};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{ToolTip, Tray, TrayMethods};
use tokio::sync::mpsc;
use zbus::zvariant::Value;

/// The server holds a feed request for this long; the client waits longer.
const FEED_WAIT: u64 = 50;
const FETCH_TIMEOUT: Duration = Duration::from_secs(70);
const ACK_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_START: u64 = 5;
const BACKOFF_MAX: u64 = 60;
/// Newest open rows kept for the menu.
const MENU_ROWS: usize = 5;
/// More new rows than this arrive as one summary popup.
const POPUP_LIMIT: usize = 3;

fn failed_units(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let first = fields.next()?;
            let unit = if first == "●" { fields.next()? } else { first };
            let _load = fields.next()?;
            (fields.next()? == "failed").then_some(unit)
        })
        .collect()
}

fn remember_failed(last: &mut HashMap<String, Instant>, units: Vec<&str>, now: Instant) {
    for unit in units {
        last.insert(unit.to_string(), now);
    }
    last.retain(|_, seen| now.duration_since(*seen) < Duration::from_secs(3600));
}

fn key_safe(key: &str) -> String {
    key.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '.' | '_' | '-') { c } else { '-' })
        .take(128)
        .collect()
}

/// The server prefixes a member's keys with `member:<name>:` and caps the
/// stored key at 128 bytes, so the folded name is cut to 80 bytes. A hash of
/// the unfolded host and unit keeps keys distinct when folding or cutting
/// would merge two units.
// ponytail: DefaultHasher is stable within a build only; a toolchain bump re-keys rows once.
fn unit_key(host: &str, unit: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (host, unit).hash(&mut hasher);
    let mut key = key_safe(&format!("unit:{host}:{unit}"));
    key.truncate(80);
    format!("{key}:{:08x}", hasher.finish() as u32)
}

async fn report_units(client: HubClient) {
    let mut last = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    loop {
        interval.tick().await;
        let result = async {
            let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")?;
            let output = tokio::time::timeout(
                Duration::from_secs(15),
                tokio::process::Command::new("systemctl")
                    .args(["--user", "list-units", "--state=failed", "--plain", "--no-legend"])
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            anyhow::ensure!(output.status.success(), "systemctl exited with {}", output.status);
            let text = std::str::from_utf8(&output.stdout)?;
            remember_failed(&mut last, failed_units(text), Instant::now());
            let items = last
                .keys()
                .map(|unit| SyncNotificationItem {
                    dedupe_key: unit_key(hostname.trim(), unit),
                    severity: Severity::Warning,
                    title: format!("{unit} failed on {}", hostname.trim()),
                    body: "A user unit has failed.".into(),
                    subject_type: None,
                    subject_id: None,
                    url: None,
                })
                .collect();
            tokio::time::timeout(
                Duration::from_secs(15),
                // Per-host source: one token may run a tray on several hosts, and a
                // sync resolves every row of its source that it does not list.
                client.sync_notifications(&SyncNotificationsIn { source: key_safe(&format!("units:{}", hostname.trim())), items }),
            )
            .await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            eprintln!("fleetix-hub-tray: unit report: {error:#}");
        }
    }
}

fn quiet_transition(held: &mut usize, active: bool, count: usize) -> Option<usize> {
    if active {
        *held += count;
        None
    } else if *held > 0 {
        Some(std::mem::take(held))
    } else {
        None
    }
}

async fn gaming() -> bool {
    matches!(tokio::time::timeout(Duration::from_secs(2), tokio::process::Command::new("gamemoded").arg("-s").kill_on_drop(true).output()).await,
        Ok(Ok(output)) if output.status.success() && String::from_utf8_lossy(&output.stdout).contains("is active"))
}

async fn release_held(notifier: Option<&Notifier>, held: &mut usize, active: bool, count: usize) {
    if let Some(count) = quiet_transition(held, active, count)
        && let Some(notifier) = notifier
    {
        notifier
            .popup(&Popup {
                title: "Fleetix Hub".into(),
                body: format!("{count} hub notifications while you were playing"),
                critical: false,
            })
            .await;
    }
}

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
    // The feed must include closed rows (acknowledged, snoozed, resolved) or a
    // cached menu entry would never leave; the startup snapshot wants open rows.
    match tokio::time::timeout(FETCH_TIMEOUT, client.notifications(after, wait, after.is_some())).await {
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
            // Bounded: a stalled hub must not stop the poll loop or Quit.
            let all = AckNotificationsIn { ids: Vec::new(), all: true };
            match tokio::time::timeout(ACK_TIMEOUT, client.ack_notifications(&all)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => eprintln!("fleetix-hub-tray: acknowledge all: {error}"),
                Err(_) => eprintln!("fleetix-hub-tray: acknowledge all: no answer within {} s", ACK_TIMEOUT.as_secs()),
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
    if std::env::var("HUB_REPORT_UNITS").as_deref() == Ok("1") {
        tokio::spawn(report_units(client.clone()));
    }
    let quiet = std::env::var("HUB_QUIET_WHILE_GAMING").as_deref() == Ok("1");
    let mut held = 0;
    let mut gaming_check = tokio::time::interval(Duration::from_secs(10));
    // The tick is only polled while popups are held; the default `Burst` would
    // replay every tick missed meanwhile and starve the feed.
    gaming_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
            _ = gaming_check.tick(), if quiet && held > 0 => {
                release_held(notifier.as_ref(), &mut held, gaming().await, 0).await;
                continue;
            }
        };
        let page = match fetched {
            Ok(page) => page,
            Err(error) => {
                // Offline: no popup. The cursor is kept, so recovery resumes the
                // feed and announces what changed meanwhile; only daemon startup
                // takes the silent snapshot.
                state.offline = true;
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
        let count = if snapshot { page.open.max(0) as usize } else { page.items.iter().filter(|item| item.open).count() };
        let popups = apply(&mut state, page);
        let _ = handle.update(|tray| tray.view = view(&state)).await;
        let active = quiet && (count > 0 || held > 0) && gaming().await;
        if quiet {
            release_held(notifier.as_ref(), &mut held, active, count).await;
        }
        if let Some(notifier) = &notifier {
            if active {
                continue;
            } else if snapshot {
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
    }

    handle.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn parses_failed_unit_lines() {
        assert_eq!(
            failed_units("  foo.service loaded failed failed Foo\n● bar.timer loaded failed failed Bar\n\n"),
            vec!["foo.service", "bar.timer"]
        );
        assert!(failed_units("").is_empty());
    }

    #[test]
    fn failed_units_remain_for_one_hour_after_last_sighting() {
        let now = Instant::now();
        let mut last = HashMap::new();
        remember_failed(&mut last, vec!["foo.service"], now);
        remember_failed(&mut last, vec![], now + Duration::from_secs(3599));
        assert!(last.contains_key("foo.service"));
        remember_failed(&mut last, vec!["foo.service"], now + Duration::from_secs(3600));
        remember_failed(&mut last, vec![], now + Duration::from_secs(7199));
        assert!(last.contains_key("foo.service"));
        remember_failed(&mut last, vec![], now + Duration::from_secs(7200));
        assert!(last.is_empty());
    }

    #[test]
    fn quiet_mode_counts_notifications_and_summarizes_once() {
        let mut held = 0;
        assert_eq!(quiet_transition(&mut held, true, 4), None);
        assert_eq!(quiet_transition(&mut held, true, 2), None);
        assert_eq!(held, 6);
        assert_eq!(quiet_transition(&mut held, false, 0), Some(6));
        assert_eq!(quiet_transition(&mut held, false, 0), None);
    }

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

    #[test]
    fn unit_keys_fit_the_member_namespace_and_stay_distinct() {
        let long = format!("{}.service", "a".repeat(200));
        let key = unit_key("murph", &long);
        assert!(format!("member:dejana:{key}").len() <= 128, "{key}");
        assert_ne!(unit_key("murph", "foo@bar.service"), unit_key("murph", "foo-bar.service"));
        assert_ne!(unit_key("murph", "x.service"), unit_key("starlord", "x.service"));
        assert_eq!(unit_key("murph", "x.service"), unit_key("murph", "x.service"));
    }
}
