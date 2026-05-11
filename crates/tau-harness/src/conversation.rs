//! Per-conversation state tracked by the harness.
//!
//! A *conversation* is one logical thread of prompts/responses
//! within a session: the user's interactive UI prompts on one branch,
//! plus any side conversations spawned by extensions via
//! [`tau_proto::ExtAgentQuery`] on their own branches. Each
//! conversation has its own local cursor (`head`) into the session
//! tree so multiple in-flight conversations can coexist without their
//! tree positions clobbering each other.
//!
//! The harness multiplexes incoming agent / tool events back to the
//! right conversation via two id maps it owns:
//! `prompt_conversations: HashMap<SessionPromptId, ConversationId>`
//! and `tool_conversations: HashMap<ToolCallId, ConversationId>`.
//! Both keys are looked up first; the conversation then yields the
//! `session_id` that the older `prompt_sessions` /
//! `pending_tool_sessions` maps used to carry directly.

use std::collections::VecDeque;

use tau_core::NodeId;
use tau_proto::{ConnectionId, PromptOriginator, SessionId, SessionPromptId, ToolCallId};

/// Opaque per-process conversation identifier. Not on the wire — the
/// harness mints these locally and uses them as routing keys.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct ConversationId(String);

impl ConversationId {
    pub(crate) fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::borrow::Borrow<str> for ConversationId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Per-conversation turn state. There is no global agent slot — the
/// agent extension serializes its own consumption of
/// `SessionPromptCreated` events. State per conversation is what gates
/// dispatch of the *next* prompt for that conversation.
#[derive(Debug, Default)]
pub(crate) enum ConversationTurnState {
    #[default]
    Idle,
    AgentThinking {
        #[allow(dead_code)]
        session_prompt_id: SessionPromptId,
    },
    ToolsRunning {
        remaining_calls: Vec<ToolCallId>,
    },
}

/// One in-flight conversation tracked by the harness.
///
/// The user's main UI thread is the *default* conversation, present
/// for the harness's whole lifetime. Side queries from extensions
/// spawn additional conversations that are removed once their final
/// response is routed back.
#[derive(Debug)]
pub(crate) struct Conversation {
    /// Owning conversation id. Duplicates the key in the harness's
    /// `conversations` map, but pinning it on the conversation itself
    /// lets future code carry a `&Conversation` without also threading
    /// the id through every call site.
    #[allow(dead_code)]
    pub(crate) id: ConversationId,
    pub(crate) session_id: SessionId,
    pub(crate) originator: PromptOriginator,
    /// Local cursor — where the *next* event for this conversation
    /// should be parented in the session tree. The tree's own `head`
    /// is whichever conversation appended last; this field is what
    /// `publish_for_conversation` snaps the tree head back to before
    /// emitting an event for this conversation.
    pub(crate) head: Option<NodeId>,
    /// For [`PromptOriginator::Extension`] conversations: the
    /// connection id of the extension that issued the
    /// [`tau_proto::ExtAgentQuery`], so the harness knows where to
    /// route the matching [`tau_proto::ExtAgentQueryResult`].
    pub(crate) source_connection: Option<ConnectionId>,
    /// Session prompt id of the prompt currently in flight for this
    /// conversation, or `None` if nothing is pending.
    pub(crate) in_flight_prompt: Option<SessionPromptId>,
    /// Per-conversation prompt queue: text waiting to be dispatched
    /// once this conversation's `turn_state` returns to `Idle`. Other
    /// conversations dispatch independently; the agent extension
    /// serializes its own consumption of `SessionPromptCreated`.
    pub(crate) pending_prompts: VecDeque<String>,
    /// Correlation tag carried in by a [`tau_proto::UiPromptSubmitted`]
    /// and copied onto the next [`tau_proto::SessionPromptCreated`] this
    /// conversation emits. Cleared once consumed. Currently only set
    /// for the synchronous dispatch path; queued prompts drop the tag,
    /// since the queue stores text only.
    pub(crate) next_ctx_id: Option<String>,
    pub(crate) turn_state: ConversationTurnState,
    /// For side conversations spawned by a tool-implementing extension
    /// (currently just `delegate`): the parent agent's tool call id
    /// that this conversation is fulfilling. Lets the harness emit
    /// [`tau_proto::DelegateProgress`] under that call id as the
    /// sub-agent runs. `None` for the default conversation and for
    /// non-tool ext-queries (e.g. notifications' idle summary).
    pub(crate) parent_tool_call_id: Option<ToolCallId>,
    /// Display name supplied by the parent agent for the delegated
    /// task, surfaced in the UI alongside `parent_tool_call_id`. Only
    /// set when `parent_tool_call_id` is.
    pub(crate) task_name: Option<String>,
    /// Number of tool calls currently in flight on this conversation.
    pub(crate) tools_in_flight: u32,
    /// Cumulative tool calls this conversation has started (in-flight
    /// + completed). Used as the `total` in `DelegateProgress`.
    pub(crate) tools_total: u32,
    /// Most recent input-token count this conversation's agent
    /// reported on a finished response. Used for `DelegateProgress`.
    pub(crate) context_input_tokens: Option<u64>,
    /// Most recent percent-of-context-window this conversation's
    /// agent has used. Computed from `context_input_tokens` and the
    /// model's window size; `None` when the window is unknown.
    pub(crate) context_percent_used: Option<u8>,
}

impl Conversation {
    pub(crate) fn new(
        id: ConversationId,
        session_id: SessionId,
        originator: PromptOriginator,
        head: Option<NodeId>,
        source_connection: Option<ConnectionId>,
    ) -> Self {
        Self {
            id,
            session_id,
            originator,
            head,
            source_connection,
            in_flight_prompt: None,
            pending_prompts: VecDeque::new(),
            next_ctx_id: None,
            turn_state: ConversationTurnState::Idle,
            parent_tool_call_id: None,
            task_name: None,
            tools_in_flight: 0,
            tools_total: 0,
            context_input_tokens: None,
            context_percent_used: None,
        }
    }
}
