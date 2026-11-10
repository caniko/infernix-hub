//! Cross-workspace navigation and the palette's action vocabulary.

use contracts::SearchHit;
use uuid::Uuid;

use crate::{
    format::short_hash,
    state::{Store, Workspace},
};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Workspace(Workspace),
    ToggleInspector,
    Refresh,
    /// Re-read the fleet snapshot now instead of waiting for the 30s cadence.
    RefreshFleet,
    /// Open a subject by contract `subject_type` (search hits, Today items).
    OpenSubject(String, Uuid),
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaletteEntry {
    pub label: String,
    pub hint: String,
    /// "navigate" | "command" | "search"
    pub kind: &'static str,
    pub action: Action,
}

pub fn static_entries() -> Vec<PaletteEntry> {
    let mut entries: Vec<PaletteEntry> = Workspace::ALL
        .iter()
        .map(|workspace| PaletteEntry {
            label: format!("Go to {}", workspace.label()),
            hint: workspace.hint().to_string(),
            kind: "navigate",
            action: Action::Workspace(*workspace),
        })
        .collect();
    entries.push(PaletteEntry {
        label: "Toggle inspector".into(),
        hint: "technical ids, payload hash, history".into(),
        kind: "command",
        action: Action::ToggleInspector,
    });
    entries.push(PaletteEntry {
        label: "Refresh now".into(),
        hint: "reload all workspaces from the hub".into(),
        kind: "command",
        action: Action::Refresh,
    });
    entries.push(PaletteEntry {
        label: "Refresh fleet status".into(),
        hint: "re-read the snapshot now (every 30s otherwise)".into(),
        kind: "command",
        action: Action::RefreshFleet,
    });
    entries
}

pub fn search_entry(hit: &SearchHit) -> PaletteEntry {
    PaletteEntry {
        label: hit.title.clone(),
        hint: format!("{} · {}", hit.subject_type, short_hash(&hit.subject_id.to_string())),
        kind: "search",
        action: Action::OpenSubject(hit.subject_type.clone(), hit.subject_id),
    }
}

/// Route a subject id to the workspace that owns it. Releases and generations
/// have no by-id read endpoint yet, so they land on Studio with a pointer.
pub fn open_subject(store: Store, subject_type: &str, id: Uuid) {
    match subject_type {
        "content" | "content_item" => {
            store.set_workspace(Workspace::Studio);
            store.set_selected_content(Some(id));
        }
        "news" | "news_item" => {
            store.set_workspace(Workspace::News);
            store.set_selected_news(Some(id));
        }
        "persona" => {
            store.set_workspace(Workspace::Library);
            store.set_selected_persona(Some(id));
        }
        "relationship" => {
            store.set_workspace(Workspace::Relationships);
            store.set_selected_relationship(Some(id));
        }
        "release" | "release_revision" => {
            store.set_workspace(Workspace::Studio);
            store.notify(
                false,
                format!(
                    "Release {} — open its content item in Studio (no release-by-id read in the contract).",
                    short_hash(&id.to_string())
                ),
            );
        }
        "generation" | "generation_request" => {
            store.set_workspace(Workspace::Studio);
            store.notify(
                false,
                format!(
                    "Generation {} — open its content item in Studio (no generation-by-id read in the contract).",
                    short_hash(&id.to_string())
                ),
            );
        }
        other => store.notify(false, format!("No workspace handles subject type “{other}”.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_entries_cover_navigation_and_commands() {
        let entries = static_entries();
        assert_eq!(entries.iter().filter(|e| e.kind == "navigate").count(), 10);
        assert!(entries.iter().any(|e| e.action == Action::ToggleInspector));
        assert!(entries.iter().any(|e| e.action == Action::Refresh));
        assert!(entries.iter().any(|e| e.action == Action::RefreshFleet));
    }
}
