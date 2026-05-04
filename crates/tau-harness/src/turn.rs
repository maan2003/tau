//! Turn-state machine governing whose turn it is in the agent loop, plus the
//! outcome enum returned when a user prompt is submitted.

use tau_proto::{SessionId, ToolCallId};

/// Tracks whose turn it is in the agent interaction loop.
pub(crate) enum TurnState {
    /// Waiting for user input (or queued prompt dispatch).
    Idle,
    /// Waiting for tool extensions to finish per-session setup
    /// (announce skills + AGENTS.md) after a `SessionStarted` broadcast,
    /// before any user prompt for that session can be dispatched.
    InitializingSession {
        session_id: SessionId,
        waiting_on: std::collections::HashSet<tau_proto::ConnectionId>,
    },
    /// Agent is processing a prompt; we are waiting for its response.
    AgentThinking { _session_id: SessionId },
    /// Agent requested tool calls; waiting for all results before
    /// sending the next prompt.
    ToolsRunning {
        session_id: SessionId,
        remaining_calls: Vec<ToolCallId>,
    },
}

impl TurnState {
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, TurnState::Idle)
    }
}

/// Outcome of `submit_user_prompt`: either the prompt was handed off to
/// the agent immediately, was placed on `pending_prompts` and will be
/// dispatched once the harness is ready (model selected, agent idle,
/// extensions ready, session initialized), or was rejected because its
/// `session_id` doesn't match the harness's bound session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PromptSubmission {
    Dispatched,
    Queued,
    Rejected { reason: String },
}
