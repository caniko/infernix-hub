//! Creative chat panel adapted from the donor's terminal-style `ChatPanel`
//! (transcript line classes + composer). Messages come from the hub's
//! `/v1/chat/{subject_type}/{id}` endpoints; the WebSocket/coroutine coupling
//! is gone, so sending is a plain event to the caller.

use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptLine {
    /// "operator" | "assistant" | "system" | "error"
    pub role: String,
    pub text: String,
    /// When the assistant attached a canon proposal, its review button label.
    pub proposal: Option<String>,
}

impl TranscriptLine {
    fn class(&self) -> &'static str {
        match self.role.as_str() {
            "operator" => "line op",
            "assistant" => "line asst",
            "error" => "line err",
            _ => "line sys",
        }
    }
}

#[component]
pub fn ChatPanel(
    lines: Vec<TranscriptLine>,
    disabled: bool,
    placeholder: String,
    on_send: EventHandler<String>,
    /// Called with the transcript index when a proposal link is clicked.
    on_review: EventHandler<usize>,
) -> Element {
    let mut input = use_signal(String::new);
    let id = use_hook(|| uuid::Uuid::new_v4().simple().to_string());
    let transcript_id = format!("chat-transcript-{id}");
    let input_id = format!("chat-input-{id}");
    let scroll = format!(
        "requestAnimationFrame(() => {{ const log = document.getElementById('{transcript_id}'); if (log) log.scrollTop = log.scrollHeight; }});"
    );
    use_effect(use_reactive((&lines,), move |_| {
        let _ = document::eval(&scroll);
    }));
    let mut submit = move || {
        let text = input.cloned().trim().to_string();
        if !text.is_empty() && !disabled {
            on_send.call(text);
            input.set(String::new());
        }
    };
    rsx! {
        section { class: "chat", aria_label: "Creative chat",
            div { class: "transcript", id: "{transcript_id}", role: "log", aria_label: "Messages", aria_live: "polite",
                for (i, line) in lines.iter().enumerate() {
                    div { class: "{line.class()}", key: "{i}",
                        "{line.text}"
                        if let Some(label) = line.proposal.clone() {
                            button {
                                class: "btn btn-glass btn-xs proposal-link",
                                onclick: move |_| on_review.call(i),
                                "{label}"
                            }
                        }
                    }
                }
                if lines.is_empty() {
                    p { class: "empty", "Send a message to start a turn. The assistant can propose canon; only you can accept it." }
                }
            }
            div { class: "composer",
                label { class: "sr-only", r#for: "{input_id}", "Message" }
                input {
                    id: "{input_id}",
                    class: "input",
                    placeholder: if disabled { "Loading…" } else { "{placeholder}" },
                    disabled,
                    value: "{input}",
                    oninput: move |e| input.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Named(NamedKey::Enter) {
                            submit();
                        }
                    },
                }
                button {
                    class: "btn btn-primary",
                    disabled: disabled || input.cloned().trim().is_empty(),
                    onclick: move |_| submit(),
                    "Send"
                }
            }
        }
    }
}
