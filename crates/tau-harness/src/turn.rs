//! Minimal global harness gate for session initialization.
//!
//! Per-conversation agent/tool progress lives on `Conversation`; this
//! type only tracks states that genuinely block the whole harness —
//! currently just per-session setup waiting on extensions.

use tau_proto::{SessionId, SessionStartReason};

/// Global harness state that is not owned by one conversation.
pub(crate) enum TurnState {
    /// Normal operation: conversations may dispatch their next prompt
    /// as soon as the model is selected and extensions are ready.
    Idle,
    /// Waiting for tool extensions to finish per-session setup
    /// (announce skills + AGENTS.md) after a `SessionStarted` broadcast,
    /// before any prompt for that session can be dispatched.
    InitializingSession {
        session_id: SessionId,
        reason: SessionStartReason,
        waiting_on: std::collections::HashSet<tau_proto::ConnectionId>,
    },
}

impl TurnState {
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, TurnState::Idle)
    }
}

/// Outcome of `submit_user_prompt`: either the prompt was handed off to
/// the agent immediately, was placed on a conversation queue and will be
/// dispatched once the harness is ready (model selected, extensions
/// ready, session initialized, conversation not already in flight), or
/// was rejected because its `session_id` doesn't match the harness's
/// bound session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PromptSubmission {
    Dispatched,
    Queued,
    Rejected { reason: String },
}
