//! Hub transport: one `HubClient` from the environment, bounded requests, and
//! the refresh routines that fan responses into the shared signals.
//!
//! `HUB_URL` defaults to `http://127.0.0.1:8470`; `HUB_TOKEN` is the operator
//! bearer token (empty when the hub runs without auth in local dev).

use std::{future::Future, time::Duration};

use contracts::{
    Asset, AssetState, ContentItem, PublicationIntent,
    client::{ClientError, HubClient},
};
use dioxus::prelude::*;
use uuid::Uuid;

use crate::{
    state::{Loaded, Store, UiError},
    theme,
};

/// Donor polling cadence (1500 ms) retained for the active console.
pub const POLL_INTERVAL_MS: u64 = 1500;
/// The fleet snapshot is rewritten every few minutes, so 30s is plenty.
pub const FLEET_POLL_SECS: u64 = 30;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
/// Cap eager asset-byte fetches per detail load; the rest load on next refresh.
const MAX_ASSET_FETCH: usize = 8;

pub fn make_client() -> HubClient {
    let base = std::env::var("HUB_URL").unwrap_or_else(|_| "http://127.0.0.1:8470".into());
    let token = std::env::var("HUB_TOKEN").unwrap_or_default();
    HubClient::new(base, token)
}

pub async fn timed<T>(future: impl Future<Output = Result<T, ClientError>>) -> Result<T, UiError> {
    match tokio::time::timeout(REQUEST_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(UiError::from_client(error)),
        Err(_) => Err(UiError::new("timeout", "the hub did not answer within 6s")),
    }
}

fn keep<T: Clone + 'static>(mut signal: Signal<Loaded<T>>, error: UiError) {
    let mut current = signal.cloned();
    current.failed(error);
    signal.set(current);
}

fn put<T: 'static>(mut signal: Signal<Loaded<T>>, value: T) {
    signal.set(Loaded::ready(value));
}

/// Refresh the fleet snapshot. A failure keeps the last good snapshot and
/// surfaces the error; the hub answers `fleet_status_unconfigured` when no
/// file is configured.
pub async fn refresh_fleet(store: Store, client: HubClient) {
    match timed(client.fleet_status()).await {
        Ok(status) => put(store.fleet, status),
        Err(error) => keep(store.fleet, error),
    }
}

/// Refresh the data every workspace can need, plus status. Runs sequentially so
/// a slow endpoint cannot stampede the hub; `poll_busy` prevents overlap.
pub async fn refresh_base(store: Store, client: HubClient) {
    if (store.poll_busy).cloned() {
        return;
    }
    store.set_poll_busy(true);

    match timed(client.status()).await {
        Ok(v) => put(store.status, v),
        Err(e) => keep(store.status, e),
    }
    match timed(client.today()).await {
        Ok(v) => put(store.today, v),
        Err(e) => keep(store.today, e),
    }
    match timed(client.news()).await {
        Ok(v) => put(store.news, v),
        Err(e) => keep(store.news, e),
    }
    match timed(client.personas()).await {
        Ok(v) => put(store.personas, v),
        Err(e) => keep(store.personas, e),
    }
    match timed(client.workflows()).await {
        Ok(v) => put(store.workflows, v),
        Err(e) => keep(store.workflows, e),
    }
    match timed(client.accounts()).await {
        Ok(v) => put(store.accounts, v),
        Err(e) => keep(store.accounts, e),
    }
    match timed(client.content()).await {
        Ok(v) => put(store.content, v),
        Err(e) => keep(store.content, e),
    }
    match timed(client.intents()).await {
        Ok(v) => put(store.intents, v),
        Err(e) => keep(store.intents, e),
    }
    match timed(client.relationships()).await {
        Ok(v) => put(store.relationships, v),
        Err(e) => keep(store.relationships, e),
    }
    match timed(client.assets()).await {
        Ok(v) => put(store.assets, v),
        Err(e) => keep(store.assets, e),
    }

    ensure_selection((store.content).cloned(), store.selected_content, |item| item.id);
    ensure_selection((store.news).cloned(), store.selected_news, |item| item.id);
    ensure_selection((store.personas).cloned(), store.selected_persona, |item| item.id);
    ensure_selection((store.relationships).cloned(), store.selected_relationship, |item| item.id);

    store.set_poll_busy(false);
}

fn ensure_selection<T: 'static>(loaded: Loaded<Vec<T>>, mut selection: Signal<Option<Uuid>>, id: impl Fn(&T) -> Uuid) {
    let Some(items) = loaded.data else { return };
    if let Some(current) = selection.cloned()
        && items.iter().any(|item| id(item) == current)
    {
        return;
    }
    selection.set(items.first().map(&id));
}

pub async fn load_content_detail(store: Store, client: HubClient, id: Uuid) {
    match timed(client.content_detail(id)).await {
        Ok(detail) => {
            store.set_content_detail(Loaded::ready(detail.clone()));
            load_asset_bytes(store, client, &detail.assets).await;
        }
        Err(error) => keep(store.content_detail, error),
    }
}

pub async fn load_asset_bytes(store: Store, client: HubClient, assets: &[Asset]) {
    let missing: Vec<Asset> = assets
        .iter()
        .filter(|asset| asset.state == AssetState::Ready)
        .filter(|asset| !store.asset_images.read().contains_key(&asset.id))
        .take(MAX_ASSET_FETCH)
        .cloned()
        .collect();
    for asset in missing {
        let path = format!("/assets/{}/content", asset.id);
        if let Ok((mime, bytes)) = timed(client.get_bytes(&path)).await {
            store.insert_asset_image(asset.id, data_uri(&mime, &bytes));
        }
    }
}

pub async fn load_relationship_detail(store: Store, client: HubClient, id: Uuid) {
    match timed(client.relationship(id)).await {
        Ok(detail) => store.set_relationship_detail(Loaded::ready(detail)),
        Err(error) => keep(store.relationship_detail, error),
    }
}

pub async fn load_canon(store: Store, client: HubClient, persona: Uuid) {
    let result = timed(client.canon(persona)).await;
    // A response for a persona that is no longer selected must not land.
    if (store.selected_persona).cloned() != Some(persona) {
        return;
    }
    match result {
        Ok(revisions) => put(store.canon, revisions),
        Err(error) => keep(store.canon, error),
    }
}

pub async fn load_chat(store: Store, client: HubClient, subject: &'static str, id: Uuid) {
    let signal = if subject == "persona" { store.chat_persona } else { store.chat_content };
    match timed(client.chat(subject, id)).await {
        Ok(messages) => put(signal, messages),
        Err(error) => keep(signal, error),
    }
}

/// Refresh the detail reads for the current selections.
pub async fn refresh_selected(store: Store, client: HubClient) {
    if let Some(id) = (store.selected_content).cloned() {
        load_content_detail(store, client.clone(), id).await;
    }
    if let Some(id) = (store.selected_relationship).cloned() {
        load_relationship_detail(store, client.clone(), id).await;
    }
    if let Some(id) = (store.selected_persona).cloned() {
        load_canon(store, client, id).await;
    }
}

/// Build the release-id → approval-state index used by Calendar. The intent
/// contract does not expose approval state, so this reads content details in
/// bounded batches (see the contract request in docs/provenance-desktop.md).
pub async fn load_release_index(store: Store, client: HubClient, content: &[ContentItem]) {
    let pending: Vec<Uuid> = content.iter().map(|item| item.id).filter(|id| !store.is_release_indexed(*id)).take(20).collect();
    for content_id in pending {
        if let Ok(detail) = timed(client.content_detail(content_id)).await {
            for release in detail.releases {
                store.index_release(release.id, content_id, release.state);
            }
            store.mark_release_indexed(content_id);
        }
    }
}

pub async fn load_search(store: Store, client: HubClient, query: String) {
    if query.trim().len() < 2 {
        store.set_search(Loaded::ready(Vec::new()));
        return;
    }
    match timed(client.search(&query)).await {
        Ok(hits) => put(store.search, hits),
        Err(error) => keep(store.search, error),
    }
}

pub fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", theme::base64(bytes))
}

/// Open a server-provided URL in the user's browser. The URL is JSON-escaped
/// before it reaches the webview, so a hostile `HandoffOut.url` cannot inject JS.
pub fn open_external(url: &str) {
    if let Ok(js_url) = serde_json::to_string(url) {
        let _ = document::eval(&format!("window.open({js_url}, '_blank', 'noopener,noreferrer');"));
    }
}

/// Count completed vs total ComfyUI prompts (one per variant) for the progress
/// bar; the Windmill procedure that submits them is not a unit of output.
pub fn attempt_progress(attempts: &[contracts::ExecutionAttempt]) -> (usize, usize) {
    let prompts: Vec<_> = attempts.iter().filter(|a| a.kind == contracts::AttemptKind::ComfyPrompt).collect();
    let done = prompts
        .iter()
        .filter(|a| {
            matches!(
                a.state,
                contracts::AttemptState::Succeeded
                    | contracts::AttemptState::Failed
                    | contracts::AttemptState::Cancelled
                    | contracts::AttemptState::OutcomeUnknown
            )
        })
        .count();
    (done, prompts.len())
}

/// Group intents by UTC day, then account, ordered by due time.
pub type IntentDay = (String, Vec<(String, Vec<PublicationIntent>)>);

pub fn group_intents(intents: &[PublicationIntent]) -> Vec<IntentDay> {
    use std::collections::BTreeMap;
    let mut days: BTreeMap<String, BTreeMap<String, Vec<PublicationIntent>>> = BTreeMap::new();
    for intent in intents {
        let day = intent.due_at.format("%Y-%m-%d").to_string();
        days.entry(day).or_default().entry(intent.account_id.clone()).or_default().push(intent.clone());
    }
    days.into_iter()
        .map(|(day, accounts)| {
            let mut accounts: Vec<_> = accounts.into_iter().collect();
            accounts.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, list) in accounts.iter_mut() {
                list.sort_by_key(|intent| intent.due_at);
            }
            (day, accounts)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use contracts::{IntentState, PublicationIntent};

    fn intent(account: &str, hour: u32) -> PublicationIntent {
        PublicationIntent {
            id: Uuid::new_v4(),
            release_revision_id: Uuid::new_v4(),
            account_id: account.into(),
            schedule_revision: 1,
            due_at: Utc.with_ymd_and_hms(2026, 11, 9, hour, 0, 0).unwrap(),
            state: IntentState::Scheduled,
            provider_post_id: None,
            provider_url: None,
            attempts: Vec::new(),
            updated_at: Utc.with_ymd_and_hms(2026, 11, 9, hour, 0, 0).unwrap(),
        }
    }

    fn attempt(state: contracts::AttemptState) -> contracts::ExecutionAttempt {
        contracts::ExecutionAttempt {
            id: Uuid::new_v4(),
            parent_id: None,
            kind: contracts::AttemptKind::ComfyPrompt,
            external_id: "ext".into(),
            engine_instance_id: None,
            state,
            progress: serde_json::json!({}),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn attempt_progress_counts_terminal_states() {
        use contracts::AttemptState;
        let root = contracts::ExecutionAttempt { kind: contracts::AttemptKind::WindmillRoot, ..attempt(AttemptState::Running) };
        let attempts = [attempt(AttemptState::Succeeded), attempt(AttemptState::Running), attempt(AttemptState::Failed), root];
        assert_eq!(attempt_progress(&attempts), (2, 3), "the Windmill root is not a prompt");
        assert_eq!(attempt_progress(&[]), (0, 0));
    }

    #[test]
    fn intents_group_by_day_then_account() {
        let mut later = intent("b", 18);
        later.due_at = Utc.with_ymd_and_hms(2026, 11, 10, 9, 0, 0).unwrap();
        let groups = group_intents(&[intent("b", 18), intent("a", 9), later]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "2026-11-09");
        assert_eq!(groups[0].1[0].0, "a");
        assert_eq!(groups[0].1[1].0, "b");
        assert_eq!(groups[1].0, "2026-11-10");
    }
}
