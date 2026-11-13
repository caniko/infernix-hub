//! Numbers and rows derived from one fleet snapshot. Pure (no Dioxus, no
//! network), like `format.rs`, so the judgements are unit-tested.

use std::collections::BTreeMap;

use contracts::fleet::{FleetStatus, Health, NightlyOutcome, PendingOp, ServiceAlert, UpdateState};

use crate::{labels, state::Workspace};

/// A snapshot older than this must never look healthy.
pub const STALE_AFTER_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub healthy: usize,
    pub hosts: usize,
    pub outdated: usize,
    pub alerts: usize,
    pub approvals: usize,
}

pub fn counts(status: &FleetStatus) -> Counts {
    let target = status.target_revision.as_deref();
    Counts {
        healthy: status.hosts.iter().filter(|host| host.health == Health::Healthy).count(),
        hosts: status.hosts.len(),
        outdated: status.hosts.iter().filter(|host| host.update_state(target) == UpdateState::Outdated).count(),
        alerts: alert_count(status),
        approvals: status.pending_ops.len(),
    }
}

/// Hosts that need the operator: not healthy, or running an older revision.
pub fn hosts_needing_attention(status: &FleetStatus) -> usize {
    let target = status.target_revision.as_deref();
    status.hosts.iter().filter(|host| host.health != Health::Healthy || host.update_state(target) == UpdateState::Outdated).count()
}

/// Alerts plus the monitoring error itself: an unreadable monitor is not
/// evidence that services are up.
pub fn alert_count(status: &FleetStatus) -> usize {
    status.monitoring.alerts.len() + usize::from(status.monitoring.error.is_some())
}

/// RFC 3339 → unix milliseconds.
pub fn parse_ms(value: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value).ok().map(|time| time.timestamp_millis().max(0) as u64)
}

/// Clock difference tolerated between the snapshot's writer and this machine.
const CLOCK_SKEW_MS: u64 = 60 * 1000;

/// `None` when the age is unknowable: an unparsable `generatedAt`, or one
/// further in the future than clock skew explains.
pub fn snapshot_age_ms(now_ms: u64, status: &FleetStatus) -> Option<u64> {
    let then = parse_ms(&status.generated_at)?;
    (then <= now_ms + CLOCK_SKEW_MS).then(|| now_ms.saturating_sub(then))
}

/// An unknowable age counts as stale: it must not look healthy.
pub fn stale(now_ms: u64, status: &FleetStatus) -> bool {
    snapshot_age_ms(now_ms, status).is_none_or(|age| age > STALE_AFTER_MS)
}

/// One row of "Needs attention"; the workspace is where the row leads.
#[derive(Debug, Clone, PartialEq)]
pub struct Attention {
    pub workspace: Workspace,
    pub title: String,
    pub detail: String,
}

pub fn attention_rows(status: &FleetStatus) -> Vec<Attention> {
    let target = status.target_revision.as_deref();
    let mut rows = Vec::new();
    for host in &status.hosts {
        let update = host.update_state(target);
        if host.health == Health::Healthy && update != UpdateState::Outdated {
            continue;
        }
        rows.push(Attention {
            workspace: Workspace::Hosts,
            title: host.name.clone(),
            detail: format!("{} · {}", labels::health(host.health).0, labels::update_state(update).0),
        });
    }
    if let Some(error) = &status.monitoring.error {
        rows.push(Attention { workspace: Workspace::Alerts, title: "Monitoring unreadable".into(), detail: error.clone() });
    }
    for alert in &status.monitoring.alerts {
        rows.push(Attention {
            workspace: Workspace::Alerts,
            title: alert.name.clone(),
            detail: if alert.group.is_empty() { "alert".into() } else { format!("alert · {}", alert.group) },
        });
    }
    for op in &status.pending_ops {
        rows.push(Attention { workspace: Workspace::Approvals, title: op.title.clone(), detail: format!("{} · approval", op.host) });
    }
    if let Some(nightly) = &status.nightly
        && nightly.failed()
    {
        let failed: Vec<&str> =
            nightly.hosts.iter().filter(|host| host.outcome == NightlyOutcome::Failed).map(|host| host.host.as_str()).collect();
        rows.push(Attention {
            workspace: Workspace::Hosts,
            title: "Nightly update failed".into(),
            detail: nightly.error.clone().unwrap_or_else(|| format!("{} failed", failed.join(", "))),
        });
    }
    rows
}

/// "3/4" for the hosts tile.
pub fn hosts_ratio(counts: Counts) -> String {
    format!("{}/{}", counts.healthy, counts.hosts)
}

/// Alerts grouped by their monitor group, group name first.
pub fn alert_groups(alerts: &[ServiceAlert]) -> Vec<(String, Vec<ServiceAlert>)> {
    let mut groups: BTreeMap<String, Vec<ServiceAlert>> = BTreeMap::new();
    for alert in alerts {
        groups.entry(alert.group.clone()).or_default().push(alert.clone());
    }
    groups.into_iter().collect()
}

/// Pending operations oldest first; an unparsable timestamp sorts last.
pub fn oldest_first(ops: &[PendingOp]) -> Vec<PendingOp> {
    let mut ordered: Vec<PendingOp> = ops.to_vec();
    ordered.sort_by_key(|op| parse_ms(&op.created).unwrap_or(u64::MAX));
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(generated_at: &str) -> FleetStatus {
        let json = format!(
            r#"{{
              "schemaVersion": 1,
              "generatedAt": "{generated_at}",
              "targetRevision": "new",
              "hosts": [
                {{"name": "atlas", "health": "healthy", "revision": "new", "checkedAt": "2026-11-10T03:09:00Z"}},
                {{"name": "nomad", "health": "degraded", "revision": "old", "failures": ["forgejo: inactive"], "checkedAt": "2026-11-10T03:09:00Z"}},
                {{"name": "murph", "health": "healthy", "checkedAt": "2026-11-10T03:09:00Z"}}
              ],
              "monitoring": {{
                "error": "relay refused",
                "monitored": 12,
                "alerts": [{{"key": "core_forgejo", "name": "forgejo", "group": "core", "since": "2026-11-10T02:00:00Z", "detail": ["[STATUS] == 200"]}}]
              }},
              "pendingOps": [
                {{"id": "op-2", "host": "nomad", "title": "restart forgejo", "reason": "unit is down", "requestedBy": "nightly", "created": "2026-11-10T03:05:00Z"}},
                {{"id": "op-1", "host": "atlas", "title": "finish activation", "created": "2026-11-10T03:00:00Z"}}
              ],
              "nightly": {{
                "startedAt": "2026-11-10T02:00:00Z",
                "revision": "new",
                "commit": "0123abc",
                "hosts": [{{"host": "nomad", "outcome": "failed", "detail": "health check failed"}}]
              }}
            }}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn counts_and_attention_rows_cover_every_concern_once() {
        let status = snapshot("2026-11-10T03:10:00Z");
        let counts = counts(&status);
        assert_eq!(counts.hosts, 3);
        assert_eq!(counts.healthy, 2);
        assert_eq!(counts.outdated, 1, "nomad runs the old revision");
        assert_eq!(counts.alerts, 2, "one alert plus the monitoring error");
        assert_eq!(counts.approvals, 2);
        assert_eq!(hosts_ratio(counts), "2/3");
        assert_eq!(hosts_needing_attention(&status), 1, "nomad is unhealthy and outdated, counted once");

        let rows = attention_rows(&status);
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["nomad", "Monitoring unreadable", "forgejo", "restart forgejo", "finish activation", "Nightly update failed"]);
        assert_eq!(rows[0].workspace, Workspace::Hosts);
        assert_eq!(rows[1].workspace, Workspace::Alerts);
        assert_eq!(rows[3].workspace, Workspace::Approvals);
        assert_eq!(rows[5].workspace, Workspace::Hosts);
        assert_eq!(rows[5].detail, "nomad failed");
        assert_eq!(status.attention(), rows.len(), "the hub's own count agrees");
    }

    #[test]
    fn snapshot_age_and_staleness() {
        let status = snapshot("2026-11-10T03:10:00Z");
        let generated = parse_ms("2026-11-10T03:10:00Z").unwrap();
        assert!(!stale(generated + 14 * 60 * 1000, &status));
        assert!(stale(generated + 16 * 60 * 1000, &status));
        assert!(stale(generated + STALE_AFTER_MS + 1, &status));
        assert_eq!(snapshot_age_ms(generated + 60_000, &status), Some(60_000));

        assert!(!stale(generated - 30_000, &status), "small clock skew is tolerated");
        assert!(stale(generated - 3_600_000, &status), "a snapshot from the future has no known age");

        let unparsable = snapshot("yesterday-ish");
        assert_eq!(snapshot_age_ms(0, &unparsable), None);
        assert!(stale(0, &unparsable), "an unreadable timestamp must not look healthy");
    }

    #[test]
    fn alerts_group_and_approvals_sort_oldest_first() {
        let status = snapshot("2026-11-10T03:10:00Z");
        let groups = alert_groups(&status.monitoring.alerts);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "core");
        assert_eq!(groups[0].1[0].name, "forgejo");

        let ordered = oldest_first(&status.pending_ops);
        assert_eq!(ordered[0].id, "op-1");
        assert_eq!(ordered[1].id, "op-2");
    }
}
