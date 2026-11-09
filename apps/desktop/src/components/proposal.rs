//! Canon-review components adapted from the donor's `ProposalCard` (three
//! modes: view / edit-and-accept / reject), `DeltaRow` (two-click confirm) and
//! `ListEditor` (add/remove a string list). The donor's persona-specific
//! helpers are replaced by display props so the same widgets serve hub canon
//! revisions and destructive cancel actions.

use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq)]
enum ProposalMode {
    View,
    Editing,
    Rejecting,
}

/// One pending canon proposal with the operator's gate actions:
/// Accept / Edit & accept / Reject. The assistant can only propose; accepting
/// is the only path that changes canon.
#[component]
pub fn ProposalCard(
    title: String,
    description: String,
    reason: Option<String>,
    /// The value offered for inline editing (`None` hides "Edit & accept").
    editable: Option<String>,
    /// Highlights the card when the chat links to this proposal.
    focused: bool,
    on_accept: EventHandler<Option<String>>,
    on_reject: EventHandler<Option<String>>,
) -> Element {
    let mut mode = use_signal(|| ProposalMode::View);
    let mut draft = use_signal(String::new);

    rsx! {
        div { class: if focused { "glass-card proposal-card focused" } else { "glass-card proposal-card" },
            div { class: "panel-head",
                h4 { "{title}" }
                span { class: "chip amber", "pending" }
            }
            p { class: "op-desc", "{description}" }
            if let Some(r) = reason {
                p { class: "label-tech reason", "“{r}”" }
            }
            match mode() {
                ProposalMode::View => rsx! {
                    div { class: "actions",
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| on_accept.call(None),
                            "Accept"
                        }
                        if let Some(v) = editable.clone() {
                            button {
                                class: "btn btn-glass",
                                onclick: move |_| {
                                    draft.set(v.clone());
                                    mode.set(ProposalMode::Editing);
                                },
                                "Edit & accept"
                            }
                        }
                        button {
                            class: "btn btn-glass danger",
                            onclick: move |_| {
                                draft.set(String::new());
                                mode.set(ProposalMode::Rejecting);
                            },
                            "Reject"
                        }
                    }
                },
                ProposalMode::Editing => rsx! {
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "proposal-edit", "Edited value" }
                        input {
                            id: "proposal-edit",
                            class: "input",
                            value: "{draft}",
                            oninput: move |e| draft.set(e.value()),
                        }
                    }
                    div { class: "actions",
                        button { class: "btn btn-glass", onclick: move |_| mode.set(ProposalMode::View), "Cancel" }
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| {
                                on_accept.call(Some(draft()));
                                mode.set(ProposalMode::View);
                            },
                            "Accept edit"
                        }
                    }
                },
                ProposalMode::Rejecting => rsx! {
                    div { class: "field-row",
                        label { class: "label-tech", r#for: "proposal-reason", "Reason (optional)" }
                        input {
                            id: "proposal-reason",
                            class: "input",
                            placeholder: "why you're rejecting…",
                            value: "{draft}",
                            oninput: move |e| draft.set(e.value()),
                        }
                    }
                    div { class: "actions",
                        button { class: "btn btn-glass", onclick: move |_| mode.set(ProposalMode::View), "Cancel" }
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| {
                                let reason = draft().trim().to_string();
                                on_reject.call(if reason.is_empty() { None } else { Some(reason) });
                                mode.set(ProposalMode::View);
                            },
                            "Confirm reject"
                        }
                    }
                },
            }
        }
    }
}

/// A canon-history / action row. When `action` is present it needs two clicks:
/// the first arms it, the second confirms. Reverting canon is not possible via
/// the contract, so the confirm pattern is used for irreversible cancels.
#[component]
pub fn DeltaRow(
    seq: String,
    category: String,
    description: String,
    action: Option<String>,
    on_confirm: EventHandler<()>,
) -> Element {
    let mut confirming = use_signal(|| false);
    rsx! {
        div { class: "delta-row",
            div { class: "delta-main",
                span { class: "chip", "#{seq}" }
                span { class: "delta-cat label-tech", "{category}" }
                span { class: "op-desc", "{description}" }
            }
            if let Some(label) = action {
                if confirming() {
                    div { class: "actions",
                        button { class: "btn btn-glass", onclick: move |_| confirming.set(false), "Cancel" }
                        button {
                            class: "btn btn-primary",
                            onclick: move |_| {
                                on_confirm.call(());
                                confirming.set(false);
                            },
                            "Confirm {label}"
                        }
                    }
                } else {
                    button {
                        class: "btn btn-glass",
                        onclick: move |_| confirming.set(true),
                        "{label}"
                    }
                }
            }
        }
    }
}

/// Add/remove editor over a shared list signal. Mutations flow straight into
/// the parent's signal, so its Save handler reads the live list. Used for
/// persona rules / avoid / world lists before submitting a canon proposal.
#[component]
pub fn ListEditor(
    title: String,
    hint: String,
    items: Signal<Vec<String>>,
    placeholder: String,
) -> Element {
    let mut items = items;
    let mut draft = use_signal(String::new);
    let add = move |_: MouseEvent| {
        let v = draft().trim().to_string();
        if !v.is_empty() {
            items.write().push(v);
            draft.set(String::new());
        }
    };
    let input_id = format!("list-editor-{}", title.replace(' ', "-").to_lowercase());
    rsx! {
        div { class: "glass-card settings-section",
            div { class: "panel-head",
                h3 { "{title}" }
                span { class: "chip", "{items().len()}" }
            }
            p { class: "label-tech", "{hint}" }
            if items().is_empty() {
                p { class: "label-tech sub", "None declared." }
            } else {
                div { class: "list-editor",
                    for (i, entry) in items().into_iter().enumerate() {
                        div { class: "chip-removable", key: "{i}",
                            span { "{entry}" }
                            button {
                                class: "chip-x",
                                aria_label: "Remove entry",
                                onclick: move |_| {
                                    items.write().remove(i);
                                },
                                "×"
                            }
                        }
                    }
                }
            }
            div { class: "add-row",
                label { class: "sr-only", r#for: "{input_id}", "Add an entry to {title}" }
                input {
                    id: "{input_id}",
                    class: "input",
                    placeholder: "{placeholder}",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            let v = draft().trim().to_string();
                            if !v.is_empty() {
                                items.write().push(v);
                                draft.set(String::new());
                            }
                        }
                    },
                }
                button { class: "btn btn-glass", onclick: add, "Add" }
            }
        }
    }
}
