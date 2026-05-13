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
use tau_proto::{
    ConnectionId, ModelId, ModelParams, PromptOriginator, SessionId, SessionPromptId, ToolCallId,
    ToolChoice, ToolDefinition,
};

use crate::dedup::ResultDedupMap;

/// Hash the per-request inputs whose drift would invalidate a Codex
/// chain (`previous_response_id`). System prompt, tool list, and model
/// params each appear on the wire on every turn; if they differ from
/// the prior turn the server's reasoning continuity can decohere
/// silently. Used by both [`ChainAnchor::request_fingerprint`] (set
/// when the anchor is minted) and the anchor-validity check before
/// sending the next prompt.
///
/// `tool_choice` is hashed too. It is serialized on the wire by both
/// Responses and Chat Completions backends; carrying a
/// `previous_response_id` across a `tool_choice` flip sends a request
/// whose non-input fields no longer match the anchored response and
/// can silently fall off the provider cache. Non-tool extension side
/// queries therefore preserve `ToolChoice::Auto` and the harness
/// enforces "no tool execution" locally instead of mutating the wire
/// request.
///
/// Domain-separated by a NUL byte between fields so e.g. a system
/// prompt ending in `"]"` can't be confused with the start of the
/// tools JSON. Field serialization failures (impossibly rare on these
/// types) collapse to empty bytes, which just means a mismatch and a
/// safe full-replay fallback.
pub(crate) fn compute_chain_fingerprint(
    system_prompt: &str,
    tools: &[ToolDefinition],
    model_params: &ModelParams,
    tool_choice: ToolChoice,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(system_prompt.as_bytes());
    hasher.update(b"\0tools:");
    hasher.update(&serde_json::to_vec(tools).unwrap_or_default());
    hasher.update(b"\0params:");
    hasher.update(&serde_json::to_vec(model_params).unwrap_or_default());
    hasher.update(b"\0tool_choice:");
    hasher.update(&serde_json::to_vec(&tool_choice).unwrap_or_default());
    *hasher.finalize().as_bytes()
}

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
    /// Stateful-chain anchor for backends that support it (currently
    /// the OpenAI Codex Responses API). Set when an agent reports a
    /// `response_id` on the previous finished turn; consumed by the
    /// next `send_prompt_to_agent_for` as a hint that the upstream
    /// call can chain off the prior turn instead of replaying the
    /// full transcript. `None` initially, after model switches, or
    /// after an edit / error invalidates the chain.
    pub(crate) chain_anchor: Option<ChainAnchor>,
    /// Per-conversation map from tool-result-content hash to the first
    /// `call_id` on this branch that produced that content. Consulted
    /// at intake of every `ToolResult` / `ToolError` to collapse a
    /// duplicate's payload into a short pointer that refers back to
    /// the original. Branch-scoped: rebuilt from
    /// [`Conversation::head`] whenever the cursor moves
    /// non-linearly. See `crate::dedup` for the full rationale.
    pub(crate) result_dedup: ResultDedupMap,
}

/// See [`Conversation::chain_anchor`].
#[derive(Clone, Debug)]
pub(crate) struct ChainAnchor {
    /// `response.id` returned by the provider on the most recent
    /// successful turn for this conversation.
    pub(crate) response_id: String,
    /// The conversation's tree cursor at the moment the anchor was
    /// captured (after the finished response was folded). The chain
    /// is valid only while the current `head` descends from this
    /// node — if a `UiNavigateTree` jumps to a different branch, the
    /// next send detects the mismatch and drops the anchor.
    pub(crate) head: Option<NodeId>,
    /// Model id that produced `response_id`. Switching models busts
    /// the chain even if the tree position is unchanged.
    pub(crate) model: ModelId,
    /// Number of assembled `ConversationMessage`s in the conversation
    /// at the moment the anchor was captured. The next send slices
    /// `messages[message_count..]` to get the new content the upstream
    /// API hasn't seen yet.
    pub(crate) message_count: usize,
    /// Blake3 fingerprint of `(system_prompt, tools, model_params)` as
    /// observed when the anchor was minted. Codex rejects (or silently
    /// misinterprets) a chained request whose non-input fields drift
    /// from the prior turn, so the next send re-hashes the same inputs
    /// and drops the anchor on mismatch — catching the divergence
    /// before the round-trip rather than after.
    pub(crate) request_fingerprint: [u8; 32],
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
            chain_anchor: None,
            result_dedup: ResultDedupMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use tau_proto::{ModelParams, ToolDefinition, ToolName};

    use super::compute_chain_fingerprint;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: ToolName::new(name),
            description: None,
            parameters: None,
        }
    }

    /// Locks in the inputs that DO matter for chain validity. If a
    /// future change drops one of these from the hash, the matching
    /// pair below stops differing and the test fails — catching the
    /// regression before any real session loses cache.
    #[test]
    fn fingerprint_changes_when_real_inputs_drift() {
        let base = compute_chain_fingerprint(
            "sys",
            &[tool("a")],
            &ModelParams::default(),
            tau_proto::ToolChoice::Auto,
        );

        assert_ne!(
            base,
            compute_chain_fingerprint(
                "sys-changed",
                &[tool("a")],
                &ModelParams::default(),
                tau_proto::ToolChoice::Auto,
            ),
            "system_prompt drift must change the fingerprint",
        );
        assert_ne!(
            base,
            compute_chain_fingerprint(
                "sys",
                &[tool("a"), tool("b")],
                &ModelParams::default(),
                tau_proto::ToolChoice::Auto,
            ),
            "tools drift must change the fingerprint",
        );
        let params = ModelParams {
            effort: tau_proto::Effort::High,
            ..ModelParams::default()
        };
        assert_ne!(
            base,
            compute_chain_fingerprint("sys", &[tool("a")], &params, tau_proto::ToolChoice::Auto),
            "model_params drift must change the fingerprint",
        );
    }

    #[test]
    fn fingerprint_changes_when_tool_choice_drifts() {
        let base = compute_chain_fingerprint(
            "sys",
            &[tool("a")],
            &ModelParams::default(),
            tau_proto::ToolChoice::Auto,
        );

        assert_ne!(
            base,
            compute_chain_fingerprint(
                "sys",
                &[tool("a")],
                &ModelParams::default(),
                tau_proto::ToolChoice::None,
            ),
            "tool_choice drift must change the fingerprint because it changes the wire request",
        );
    }

    #[test]
    fn fingerprint_is_stable_across_repeated_calls() {
        // Whatever inputs `compute_chain_fingerprint` accepts must
        // produce the same hash when called twice with the same
        // values. Guards against accidental nondeterminism (e.g. if
        // someone reaches for `HashMap` serialization for tools).
        let a = compute_chain_fingerprint(
            "sys",
            &[tool("a")],
            &ModelParams::default(),
            tau_proto::ToolChoice::Auto,
        );
        let b = compute_chain_fingerprint(
            "sys",
            &[tool("a")],
            &ModelParams::default(),
            tau_proto::ToolChoice::Auto,
        );
        assert_eq!(a, b, "fingerprint must be deterministic");
    }
}
