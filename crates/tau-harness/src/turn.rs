//! Minimal global harness gate.
//!
//! Per-conversation agent/tool progress lives on `Agent`; this
//! type only tracks states that genuinely block the whole harness —
//! currently no global blocking state beyond idle/non-idle bookkeeping.

/// Global harness state that is not owned by one conversation.
pub(crate) enum TurnState {
    /// Normal operation: agents may dispatch their next prompt
    /// as soon as the model is selected and extensions are ready.
    Idle,
}

impl TurnState {
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, TurnState::Idle)
    }
}

/// Outcome of `submit_user_prompt`: either the prompt was handed off to
/// the agent immediately, was placed on a conversation queue and will be
/// dispatched once the harness is ready (model selected, extensions
/// ready, startup initialized, conversation not already in flight), or
/// was rejected by prompt validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PromptSubmission {
    Dispatched,
    Queued,
    Rejected { reason: String },
}
