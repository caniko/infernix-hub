//! Notifications: what the hub's producers and agents raised. The feed is the
//! server-held long poll from `api::watch_notifications`, so this view never
//! polls on its own; `Acknowledge`, `Snooze` and `Mark renewed` are the only
//! writes and they all re-read the set afterwards.

use std::collections::HashMap;

use contracts::{Notification, Renewal, client::HubClient};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    actions, api,
    components::{Chip, EmptyState, ErrorCard, Loading, Panel},
    format, labels,
    state::Store,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Open,
    All,
}

/// Rows in display order for one filter.
///
/// The newest summary is pinned: it is the day's digest, not a condition, and
/// it must stay readable while the conditions below it come and go. Everything
/// else is open before acknowledged/resolved, then most recently changed first
/// (`seq` is the hub's change order, the same order the feed delivers).
pub fn rows(items: &HashMap<Uuid, Notification>, filter: Filter) -> Vec<Notification> {
    let mut rows: Vec<Notification> = items.values().filter(|n| filter == Filter::All || n.open).cloned().collect();
    let pinned = rows.iter().filter(|n| n.source == "summary").max_by_key(|n| n.seq).map(|n| n.id);
    rows.sort_by(|a, b| {
        let (a_pinned, b_pinned) = (Some(a.id) == pinned, Some(b.id) == pinned);
        b_pinned.cmp(&a_pinned).then(b.open.cmp(&a.open)).then(b.seq.cmp(&a.seq))
    });
    rows
}

/// The renewal a notification points at. The producer may carry the renewal's
/// key or its id, so both are matched.
pub fn renewal_for<'a>(renewals: &'a [Renewal], subject_id: Option<&str>) -> Option<&'a Renewal> {
    let subject = subject_id?;
    renewals.iter().find(|renewal| renewal.key == subject || renewal.id.to_string() == subject)
}

#[component]
pub fn NotificationsView() -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let mut filter = use_signal(|| Filter::Open);
    let feed = (store.notifications).cloned();
    let renewals = (store.renewals).cloned();

    // Renewal expansions read `GET /v1/renewals`; one read per visit suffices.
    let renewals_client = client.clone();
    use_future(move || {
        let client = renewals_client.clone();
        async move { api::refresh_renewals(store, client).await }
    });

    let list = rows(&feed.items, filter.cloned());
    let current = filter.cloned();
    let open = feed.open.max(0);
    let open_tone = (if open > 0 { "warn" } else { "muted" }).to_string();
    let empty_title = (if current == Filter::Open { "Nothing open." } else { "No notifications yet." }).to_string();
    let client_ack_all = client.clone();
    let ack_all = move |_| {
        let client = client_ack_all.clone();
        spawn(api::ack_all_notifications(store, client));
    };

    rsx! {
        main { class: "work",
            div { class: "work-head",
                div {
                    h1 { "Notifications" }
                    p { class: "hint", "Conditions the hub wants you to see; the feed wakes on change, it does not poll." }
                }
                div { class: "spacer" }
                Chip { label: format!("{open} open"), tone: Some(open_tone) }
                if feed.critical > 0 {
                    Chip { label: format!("{} critical", feed.critical), tone: Some("bad".to_string()) }
                }
                button { class: "btn btn-glass", disabled: open == 0, onclick: ack_all, "Acknowledge all" }
            }
            div { class: "row",
                button {
                    class: if current == Filter::Open { "btn btn-primary btn-xs" } else { "btn btn-glass btn-xs" },
                    onclick: move |_| filter.set(Filter::Open),
                    "Open"
                }
                button {
                    class: if current == Filter::All { "btn btn-primary btn-xs" } else { "btn btn-glass btn-xs" },
                    onclick: move |_| filter.set(Filter::All),
                    "All"
                }
                span { class: "label-tech", "{list.len()} shown" }
                if let Some(error) = renewals.error.clone() {
                    span { class: "label-tech bad", "renewals: {error.code}" }
                }
            }
            if let Some(error) = feed.feed_error.clone() {
                ErrorCard { code: error.code, message: error.message }
            }
            Panel { title: "Feed".to_string(), caption: Some(format!("{} shown", list.len())),
                if !feed.loaded && feed.feed_error.is_none() {
                    Loading { text: "Connecting to the notification feed…".to_string() }
                } else if list.is_empty() {
                    EmptyState {
                        title: empty_title,
                        hint: Some("Producers and agents raise rows here; a change wakes the feed at once.".to_string()),
                    }
                } else {
                    div { class: "stack",
                        for notification in list.iter() {
                            NotificationRow { key: "{notification.id}", notification: notification.clone() }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn NotificationRow(notification: Notification) -> Element {
    let store = use_context::<Store>();
    let client = use_context::<HubClient>();
    let renewals = (store.renewals).cloned();
    let renewal = if notification.subject_type.as_deref() == Some("renewal") {
        renewals.data.as_deref().and_then(|list| renewal_for(list, notification.subject_id.as_deref())).cloned()
    } else {
        None
    };

    let (severity_label, severity_tone) = labels::severity(notification.severity);
    let age = format::rel_time(notification.created_at.timestamp_millis().max(0) as u64);
    let id = notification.id;

    let client_ack = client.clone();
    let ack = move |_| {
        let client = client_ack.clone();
        spawn(api::ack_notification(store, client, id));
    };
    let client_day = client.clone();
    let snooze_day = move |_| {
        let client = client_day.clone();
        spawn(api::snooze_notification(store, client, id, 1));
    };
    let client_week = client.clone();
    let snooze_week = move |_| {
        let client = client_week.clone();
        spawn(api::snooze_notification(store, client, id, 7));
    };

    let subject = notification.subject_type.clone().zip(notification.subject_id.clone());
    // A renewal or opportunity has no workspace of its own; the renewal expands
    // below and `hubctl` covers the rest (same rule as Today's rows).
    let subject = subject.filter(|(kind, _)| kind != "renewal" && kind != "opportunity");

    let needs_due = renewal.as_ref().is_some_and(|renewal| renewal.cadence_days.is_none());
    let mut due = use_signal(String::new);
    let renewal_key = renewal.as_ref().map(|renewal| renewal.key.clone());
    let client_mark = client.clone();
    let mark_renewed = move |_| {
        let Some(key) = renewal_key.clone() else { return };
        let due_at = if needs_due {
            match format::parse_local(&due.cloned()) {
                Some(at) => Some(at),
                None => {
                    store.notify(false, "This renewal has no cadence; pick a due date first.");
                    return;
                }
            }
        } else {
            None
        };
        let client = client_mark.clone();
        spawn(api::mark_renewed(store, client, key, due_at));
    };

    rsx! {
        div { class: "glass-card notification-row",
            div { class: "spread",
                div { class: "row",
                    Chip { label: severity_label.to_string(), tone: Some(severity_tone.to_string()) }
                    span { class: "notification-title", "{notification.title}" }
                    if !notification.open {
                        Chip { label: "acknowledged".to_string(), tone: Some("muted".to_string()) }
                    }
                }
                span { class: "label-tech", "{age}" }
            }
            if !notification.body.is_empty() {
                // ponytail: no markdown renderer; bodies (the summary's lines)
                // are pre-wrapped plain text.
                p { class: "line", "{notification.body}" }
            }
            div { class: "row",
                if notification.open {
                    button { class: "btn btn-glass btn-xs", onclick: ack, "Acknowledge" }
                    button { class: "btn btn-glass btn-xs", onclick: snooze_day, "Snooze 1 d" }
                    button { class: "btn btn-glass btn-xs", onclick: snooze_week, "Snooze 1 w" }
                }
                if let Some((subject_type, raw_id)) = subject {
                    button {
                        class: "btn btn-glass btn-xs",
                        onclick: move |_| match Uuid::parse_str(&raw_id) {
                            Ok(id) => actions::open_subject(store, &subject_type, id),
                            Err(_) => store.notify(false, "This notification's subject is not an id."),
                        },
                        "Open"
                    }
                }
            }
            if notification.subject_type.as_deref() == Some("renewal") && renewal.is_none() {
                p { class: "label-tech", "Renewal details are unavailable right now." }
            }
            if let Some(renewal) = renewal.as_ref() {
                div { class: "renewal-detail",
                    div { class: "row",
                        span { class: "label-tech", "renewal" }
                        span { class: "label-tech", "{renewal.key}" }
                        div { class: "spacer" }
                        if let Some(at) = renewal.due_at {
                            span { class: "label-tech", "due {format::fmt_dt(at)}" }
                        } else {
                            span { class: "label-tech", "no due date yet" }
                        }
                        if let Some(days) = renewal.cadence_days {
                            span { class: "label-tech", "every {days} d" }
                        }
                    }
                    p { class: "line", "{renewal.procedure}" }
                    div { class: "row",
                        for (index, url) in renewal.urls.iter().enumerate() {
                            {
                                let url = url.clone();
                                rsx! {
                                    button {
                                        class: "btn btn-glass btn-xs",
                                        title: "{url}",
                                        onclick: move |_| api::open_external(&url),
                                        "Open page {index + 1}"
                                    }
                                }
                            }
                        }
                        if needs_due {
                            input {
                                class: "input",
                                r#type: "datetime-local",
                                aria_label: "New due date (UTC)",
                                value: "{due}",
                                oninput: move |event| due.set(event.value()),
                            }
                        }
                        button { class: "btn btn-primary btn-xs", onclick: mark_renewed, "Mark renewed" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use contracts::Severity;

    fn notification(id: Uuid, source: &str, seq: i64, open: bool) -> Notification {
        Notification {
            id,
            seq,
            source: source.into(),
            dedupe_key: format!("{source}:test:{seq}"),
            severity: Severity::Info,
            title: format!("{source} {seq}"),
            body: String::new(),
            subject_type: None,
            subject_id: None,
            url: None,
            read_at: (!open).then(|| Utc.with_ymd_and_hms(2026, 11, 15, 8, 0, 0).unwrap()),
            resolved_at: None,
            snoozed_until: None,
            created_at: Utc.with_ymd_and_hms(2026, 11, 15, 7, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 11, 15, 7, 0, 0).unwrap(),
            open,
        }
    }

    fn set(items: Vec<Notification>) -> HashMap<Uuid, Notification> {
        items.into_iter().map(|item| (item.id, item)).collect()
    }

    #[test]
    fn the_newest_summary_is_pinned_and_open_rows_come_first() {
        let summary_id = Uuid::new_v4();
        let older_summary = Uuid::new_v4();
        let open_id = Uuid::new_v4();
        let acked_id = Uuid::new_v4();
        let items = set(vec![
            notification(acked_id, "today", 90, false),
            notification(open_id, "fleet", 50, true),
            notification(older_summary, "summary", 10, true),
            notification(summary_id, "summary", 40, true),
        ]);

        let all = rows(&items, Filter::All);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].id, summary_id, "the newest summary leads");
        assert_eq!(all[1].id, open_id, "open conditions before acknowledged ones");
        assert_eq!(all[2].id, older_summary, "then newest change first among open rows");
        assert_eq!(all[3].id, acked_id, "an acknowledged row comes last");

        let open = rows(&items, Filter::Open);
        assert_eq!(open.len(), 3);
        assert!(open.iter().all(|row| row.open));
        assert_eq!(open[0].id, summary_id);
        assert_eq!(open[1].id, open_id);
    }

    #[test]
    fn renewal_lookup_accepts_key_or_id() {
        let renewal = Renewal {
            id: Uuid::new_v4(),
            company_id: "caniko".into(),
            key: "github-pat".into(),
            account_id: None,
            title: "GitHub PAT".into(),
            procedure: "1. Mint".into(),
            urls: vec!["https://github.com/settings/tokens".into()],
            cadence_days: Some(90),
            warn_days: 14,
            due_at: None,
            renewed_at: None,
            created_at: Utc.with_ymd_and_hms(2026, 11, 15, 7, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 11, 15, 7, 0, 0).unwrap(),
        };
        let list = [renewal.clone()];
        let id = renewal.id.to_string();
        assert_eq!(renewal_for(&list, Some("github-pat")).map(|r| r.id), Some(renewal.id));
        assert_eq!(renewal_for(&list, Some(&id)).map(|r| r.id), Some(renewal.id));
        assert!(renewal_for(&list, Some("other")).is_none());
        assert!(renewal_for(&list, None).is_none());
    }
}
