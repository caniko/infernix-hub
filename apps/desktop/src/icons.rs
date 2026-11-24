//! Line icons, some copied from the donor's `assets/icons/*.svg`
//! (MIT; no Aivyx logos or wordmarks). Rendered inline so `currentColor`
//! inherits from the surrounding control and no asset bundler is needed.

use crate::Workspace;

const COMMAND: &str = include_str!("../assets/icons/command-center.svg");
const DOCUMENTS: &str = include_str!("../assets/icons/documents.svg");
const CHAT: &str = include_str!("../assets/icons/chat.svg");
const MEMORY: &str = include_str!("../assets/icons/memory.svg");
const AGENTS: &str = include_str!("../assets/icons/agents.svg");
const SETTINGS: &str = include_str!("../assets/icons/settings.svg");
const USAGE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4 4v16h16M8 15l4-5 4 2 4-6"/></svg>"#;
const BELL: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9M10 21h4"/></svg>"#;
const ALERT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M10.3 4.6 2.5 18a1.6 1.6 0 0 0 1.4 2.4h16.2a1.6 1.6 0 0 0 1.4-2.4L13.7 4.6a2 2 0 0 0-3.4 0ZM12 9v4m0 4h.01"/></svg>"#;
const CALENDAR: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="16" rx="2"/><path d="M3 10h18M8 3v4m8-4v4m-8 7h2m4 0h2m-8 3h2"/></svg>"#;
const MAIL: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><path d="m3 7 9 6 9-6"/></svg>"#;
const SUN: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2m0 16v2M2 12h2m16 0h2M5 5l1.5 1.5m11 11L19 19M5 19l1.5-1.5m11-11L19 5"/></svg>"#;
const SPARKLES: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="m10 3 2 6 6 2-6 2-2 6-2-6-6-2 6-2 2-6Zm9 11v6m-3-3h6m-3-14v4m-2-2h4"/></svg>"#;

/// The nav icon for a workspace.
pub fn workspace(w: Workspace) -> &'static str {
    match w {
        Workspace::Notifications => BELL,
        Workspace::Command => COMMAND,
        Workspace::Hosts => SETTINGS,
        Workspace::Alerts => ALERT,
        Workspace::Approvals => AGENTS,
        Workspace::Providers => USAGE,
        Workspace::Today => SUN,
        Workspace::News => DOCUMENTS,
        Workspace::Studio => SPARKLES,
        Workspace::Calendar => CALENDAR,
        Workspace::Mail => MAIL,
        Workspace::Relationships => CHAT,
        Workspace::Projects => AGENTS,
        Workspace::Library => MEMORY,
    }
}

#[allow(dead_code)]
pub const PERSONA: &str = AGENTS;
#[allow(dead_code)]
pub const INSPECTOR: &str = SETTINGS;
