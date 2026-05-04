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
use tau_proto::{ConnectionId, PromptOriginator, SessionId, SessionPromptId};

/// Opaque per-process conversation identifier. Not on the wire — the
/// harness mints these locally and uses them as routing keys.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct ConversationId(String);

impl ConversationId {
    pub(crate) fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[allow(dead_code)] // used by the ext-query path wired in a follow-up step
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

/// One in-flight conversation tracked by the harness.
///
/// The user's main UI thread is the *default* conversation, present
/// for the harness's whole lifetime. Side queries from extensions
/// spawn additional conversations that are removed once their final
/// response is routed back.
#[allow(dead_code)] // fields populated by the ext-query path wired in a follow-up step
#[derive(Debug)]
pub(crate) struct Conversation {
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
    /// when the agent slot frees up.
    pub(crate) pending_prompts: VecDeque<String>,
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
        }
    }
}
