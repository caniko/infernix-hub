//! Generic line icons copied from the donor's `assets/icons/*.svg`
//! (MIT; no Aivyx logos or wordmarks). Rendered inline so `currentColor`
//! inherits from the surrounding control and no asset bundler is needed.

use crate::Workspace;

const COMMAND: &str = include_str!("../assets/icons/command-center.svg");
const DOCUMENTS: &str = include_str!("../assets/icons/documents.svg");
const MISSIONS: &str = include_str!("../assets/icons/missions.svg");
const CHANNELS: &str = include_str!("../assets/icons/channels.svg");
const CHAT: &str = include_str!("../assets/icons/chat.svg");
const MEMORY: &str = include_str!("../assets/icons/memory.svg");
const AGENTS: &str = include_str!("../assets/icons/agents.svg");
const SETTINGS: &str = include_str!("../assets/icons/settings.svg");
const USAGE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4 4v16h16M8 15l4-5 4 2 4-6"/></svg>"#;

/// The nav icon for a workspace.
pub fn workspace(w: Workspace) -> &'static str {
    match w {
        Workspace::Notifications => CHANNELS,
        Workspace::Command => COMMAND,
        Workspace::Hosts => SETTINGS,
        Workspace::Alerts => CHANNELS,
        Workspace::Approvals => AGENTS,
        Workspace::Providers => USAGE,
        Workspace::Today => MISSIONS,
        Workspace::News => DOCUMENTS,
        Workspace::Studio => MISSIONS,
        Workspace::Calendar => CHANNELS,
        Workspace::Relationships => CHAT,
        Workspace::Projects => AGENTS,
        Workspace::Library => MEMORY,
    }
}

#[allow(dead_code)]
pub const PERSONA: &str = AGENTS;
#[allow(dead_code)]
pub const INSPECTOR: &str = SETTINGS;
