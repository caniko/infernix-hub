//! Human labels and chip tones for every wire enum. Chips always render the
//! label as text; the tone only colours the background.

use contracts::{
    AdapterMode, AssetState, AttemptKind, AttemptState, CanonState, ContentState, Direction, DraftState, GenerationState, IntentState,
    Owner, ReleaseState, Verification,
};

#[cfg(test)]
pub const TONES: &[&str] = &["ok", "warn", "bad", "info", "muted", "amber", "sage", "error"];

pub fn content_state(state: ContentState) -> (&'static str, &'static str) {
    match state {
        ContentState::Idea => ("Idea", "muted"),
        ContentState::Brief => ("Brief", "info"),
        ContentState::Producing => ("Producing", "warn"),
        ContentState::Selecting => ("Selecting", "info"),
        ContentState::Review => ("In review", "amber"),
        ContentState::Scheduled => ("Scheduled", "info"),
        ContentState::Published => ("Published", "ok"),
        ContentState::Archived => ("Archived", "muted"),
    }
}

pub fn generation_state(state: GenerationState) -> (&'static str, &'static str) {
    match state {
        GenerationState::Requested => ("Requested", "info"),
        GenerationState::Dispatched => ("Dispatched", "info"),
        GenerationState::Running => ("Running", "warn"),
        GenerationState::Succeeded => ("Succeeded", "ok"),
        GenerationState::Failed => ("Failed", "bad"),
        GenerationState::Cancelling => ("Cancelling", "warn"),
        GenerationState::Cancelled => ("Cancelled", "muted"),
        GenerationState::OutcomeUnknown => ("Outcome unknown", "bad"),
    }
}

pub fn attempt_state(state: AttemptState) -> (&'static str, &'static str) {
    match state {
        AttemptState::Submitted => ("Submitted", "info"),
        AttemptState::Queued => ("Queued", "info"),
        AttemptState::Running => ("Running", "warn"),
        AttemptState::Succeeded => ("Succeeded", "ok"),
        AttemptState::Failed => ("Failed", "bad"),
        AttemptState::Cancelled => ("Cancelled", "muted"),
        AttemptState::OutcomeUnknown => ("Outcome unknown", "bad"),
    }
}

pub fn attempt_kind(kind: AttemptKind) -> &'static str {
    match kind {
        AttemptKind::WindmillRoot => "windmill root",
        AttemptKind::WindmillChild => "windmill child",
        AttemptKind::ComfyPrompt => "comfy prompt",
    }
}

pub fn asset_state(state: AssetState) -> (&'static str, &'static str) {
    match state {
        AssetState::Importing => ("Importing", "info"),
        AssetState::Ready => ("Ready", "ok"),
        AssetState::Failed => ("Failed", "bad"),
    }
}

pub fn release_state(state: ReleaseState) -> (&'static str, &'static str) {
    match state {
        ReleaseState::Draft => ("Draft", "muted"),
        ReleaseState::InReview => ("In review", "amber"),
        ReleaseState::Approved => ("Approved", "ok"),
        ReleaseState::Rejected => ("Rejected", "bad"),
        ReleaseState::Superseded => ("Superseded", "muted"),
    }
}

pub fn intent_state(state: IntentState) -> (&'static str, &'static str) {
    match state {
        IntentState::Scheduled => ("Scheduled", "info"),
        IntentState::Claimed => ("Claimed", "info"),
        IntentState::Submitting => ("Submitting", "warn"),
        IntentState::Accepted => ("Accepted", "ok"),
        IntentState::Published => ("Published", "ok"),
        IntentState::Failed => ("Failed", "bad"),
        IntentState::OutcomeUnknown => ("Outcome unknown", "bad"),
        IntentState::Cancelled => ("Cancelled", "muted"),
    }
}

pub fn draft_state(state: DraftState) -> (&'static str, &'static str) {
    match state {
        DraftState::Draft => ("Draft", "muted"),
        DraftState::InReview => ("In review", "amber"),
        DraftState::Approved => ("Approved", "ok"),
        DraftState::Queued => ("Queued", "info"),
        DraftState::Sending => ("Sending", "warn"),
        DraftState::Sent => ("Sent", "ok"),
        DraftState::Invalidated => ("Invalidated", "muted"),
        DraftState::OutcomeUnknown => ("Outcome unknown", "bad"),
        DraftState::HandedOff => ("Handed off", "info"),
        DraftState::Rejected => ("Rejected", "bad"),
    }
}

pub fn canon_state(state: CanonState) -> (&'static str, &'static str) {
    match state {
        CanonState::Proposed => ("Proposed", "amber"),
        CanonState::Accepted => ("Accepted", "ok"),
        CanonState::Rejected => ("Rejected", "bad"),
        CanonState::Superseded => ("Superseded", "muted"),
    }
}

pub fn owner(owner: Owner) -> (&'static str, &'static str) {
    match owner {
        Owner::Agent => ("Agent owned", "info"),
        Owner::Human => ("Human owned", "ok"),
    }
}

pub fn direction(direction: Direction) -> (&'static str, &'static str) {
    match direction {
        Direction::Inbound => ("Inbound", "info"),
        Direction::Outbound => ("Outbound", "amber"),
    }
}

pub fn verification(value: Verification) -> (&'static str, &'static str) {
    match value {
        Verification::Built => ("built", "muted"),
        Verification::FixtureTested => ("fixture tested", "warn"),
        Verification::ContractVerified => ("contract verified", "info"),
        Verification::LiveVerified => ("live verified", "ok"),
    }
}

pub fn adapter_mode(mode: AdapterMode) -> (&'static str, &'static str) {
    match mode {
        AdapterMode::Fixture => ("fixture", "warn"),
        AdapterMode::Live => ("live", "ok"),
        AdapterMode::Disabled => ("disabled", "bad"),
    }
}

/// Tone for a free-form `TodayItem.status` / `HistoryEntry` status string.
pub fn status_tone(status: &str) -> &'static str {
    match status.to_ascii_lowercase().as_str() {
        "failed" | "error" | "outcome_unknown" | "rejected" => "bad",
        "pending" | "review" | "in_review" | "running" | "producing" | "overdue" | "due" => "warn",
        "published" | "approved" | "accepted" | "succeeded" | "sent" | "ok" => "ok",
        "scheduled" | "claimed" | "queued" | "submitting" | "info" => "info",
        _ => "muted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(label: &str, tone: &str) {
        assert!(!label.is_empty());
        assert!(TONES.contains(&tone), "unknown tone {tone}");
    }

    #[test]
    fn every_wire_enum_has_a_text_label() {
        for s in ContentState::ALL {
            check(content_state(*s).0, content_state(*s).1);
        }
        for s in GenerationState::ALL {
            check(generation_state(*s).0, generation_state(*s).1);
        }
        for s in AttemptState::ALL {
            check(attempt_state(*s).0, attempt_state(*s).1);
        }
        for s in AssetState::ALL {
            check(asset_state(*s).0, asset_state(*s).1);
        }
        for s in ReleaseState::ALL {
            check(release_state(*s).0, release_state(*s).1);
        }
        for s in IntentState::ALL {
            check(intent_state(*s).0, intent_state(*s).1);
        }
        for s in DraftState::ALL {
            check(draft_state(*s).0, draft_state(*s).1);
        }
        for s in CanonState::ALL {
            check(canon_state(*s).0, canon_state(*s).1);
        }
        for s in Owner::ALL {
            check(owner(*s).0, owner(*s).1);
        }
        for s in Direction::ALL {
            check(direction(*s).0, direction(*s).1);
        }
        for s in Verification::ALL {
            check(verification(*s).0, verification(*s).1);
        }
        for s in AdapterMode::ALL {
            check(adapter_mode(*s).0, adapter_mode(*s).1);
        }
        for k in AttemptKind::ALL {
            assert!(!attempt_kind(*k).is_empty());
        }
        assert_eq!(status_tone("failed"), "bad");
        assert_eq!(status_tone("PUBLISHED"), "ok");
        assert_eq!(status_tone("mystery"), "muted");
    }
}
