use dioxus::prelude::*;

/// A compact empty state, or a bounded hero when supplied an icon or actions.
#[component]
pub fn EmptyState(title: String, hint: Option<String>, icon: Option<String>, actions: Option<Element>) -> Element {
    let hero = icon.is_some() || actions.is_some();
    rsx! {
        div { class: if hero { "empty-hero" } else { "empty glass-card" },
            if let Some(icon) = icon {
                span { class: "empty-icon", aria_hidden: "true", "{icon}" }
            }
            if hero {
                h3 { "{title}" }
            } else {
                p { "{title}" }
            }
            if let Some(hint) = hint {
                p { class: "muted", "{hint}" }
            }
            if let Some(actions) = actions {
                div { class: "row empty-actions", {actions} }
            }
        }
    }
}
