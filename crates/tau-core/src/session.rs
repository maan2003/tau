//! Tree-structured session history types and the persisted-event
//! record they are derived from.
//!
//! The on-disk source of truth is the per-session protocol-event log
//! ([`PersistedSessionEvent`] / `events.cbor`); the in-memory
//! [`SessionTree`] is built from it via [`SessionTree::from_events`]
//! and kept in sync incrementally by [`SessionTree::apply_event`]. No
//! other API mutates the tree, so the on-disk log and the cached
//! view cannot drift.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tau_proto::{
    ConnectionId, ContentPart, ContextItem, ContextRole, Event, LogEventId, MessageItem,
    PromptOriginator, ProviderBackend, ProviderTokenUsage, SessionId, ToolBackgroundError,
    ToolBackgroundResult, ToolCallId, ToolCallItem, ToolName, ToolResultItem, ToolResultKind,
    ToolResultStatus, ToolType, UnixMicros,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEventValidationError {
    message: String,
}

impl SessionEventValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SessionEventValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SessionEventValidationError {}

/// Default starting `LogEventId` for a tree with no events.
const FIRST_EVENT_ID: u64 = 0;

/// One persisted chat or tool activity entry belonging to a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SessionEntry {
    UserInput {
        items: Vec<ContextItem>,
    },
    AssistantResponse {
        provider_response_id: Option<String>,
        backend: Option<ProviderBackend>,
        output_items: Vec<ContextItem>,
        usage: Option<ProviderTokenUsage>,
    },
    ToolResults {
        items: Vec<ToolResultItem>,
    },
    Compaction {
        replacement_window: Vec<ContextItem>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
struct PendingToolRound {
    assistant_node_id: NodeId,
    call_order: Vec<ToolCallId>,
    terminal_results: HashMap<ToolCallId, ToolResultItem>,
}

/// A synthetic provider placeholder that moved a tool call to the background.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackgroundToolPlaceholder {
    /// Tool call id whose provider round was closed by the placeholder.
    pub call_id: ToolCallId,
    /// Model-visible tool name recorded on the placeholder.
    pub tool_name: ToolName,
    /// Tool type recorded on the placeholder.
    pub tool_type: ToolType,
    /// Prompt originator recorded on the placeholder.
    pub originator: PromptOriginator,
}

/// Durable completion, if any, for a backgrounded tool call.
#[derive(Clone, Debug, PartialEq)]
pub enum BackgroundToolCompletion {
    /// The backgrounded tool eventually returned successfully.
    Result(ToolBackgroundResult),
    /// The backgrounded tool eventually returned an error.
    Error(ToolBackgroundError),
}

/// Background state reconstructed from durable events for one tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundToolCallState {
    /// The placeholder that closed the provider-visible tool round.
    pub placeholder: BackgroundToolPlaceholder,
    /// The later real background completion, when one is present.
    pub completion: Option<BackgroundToolCompletion>,
}

// `NodeId` lives on the wire (tree-folding events carry their own
// `parent_node_id`), so the canonical definition moved to
// `tau-proto`. Re-exported here for ergonomic backward compatibility
// with existing `tau_core::NodeId` consumers.
pub use tau_proto::NodeId;

/// One node in the session tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionNode {
    pub id: NodeId,
    pub parent_id: Option<NodeId>,
    pub entry: SessionEntry,
}

/// Tree-structured session history with branching.
///
/// Each entry is a node with a unique id and parent pointer. The
/// `head` tracks the *write cursor* — where the next append will
/// land. Branching = moving the cursor back to an earlier node; the
/// next append creates a new branch off that node. There is only ever
/// one cursor; multiple "branch tips" are derived as the leaves of
/// the tree (see [`SessionTree::leaves`]).
///
/// The tree is never mutated through any imperative API on this type
/// from outside `tau-core`; it is built by folding the per-session
/// durable event log via [`SessionTree::from_events`] /
/// [`SessionTree::apply_event`]. That keeps a single source of truth
/// (the event log on disk) and removes the possibility for the tree
/// and the events log to disagree.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionTree {
    pub(crate) session_id: SessionId,
    pub(crate) nodes: Vec<SessionNode>,
    pub(crate) head: Option<NodeId>,
    /// Id the next durable event appended to this session's log
    /// should receive. Cached here so that
    /// [`SessionStore::append_session_event_at`] doesn't have to
    /// re-decode the entire on-disk log on every write to look at
    /// the last id (the previous behaviour was O(N) per append,
    /// quadratic over a long session).
    pub(crate) next_event_id: LogEventId,
    pending_tool_rounds: HashMap<NodeId, PendingToolRound>,
    tool_call_rounds: HashMap<ToolCallId, NodeId>,
}

impl SessionTree {
    /// Returns the session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the current head node id, if any.
    ///
    /// This is the *write cursor* — where the next append from a
    /// folded event will be parented. To enumerate the tips of every
    /// existing branch, use [`SessionTree::leaves`] instead.
    #[must_use]
    pub fn head(&self) -> Option<NodeId> {
        self.head
    }

    /// Returns a node by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&SessionNode> {
        self.nodes.get(id.get() as usize)
    }

    /// Returns all nodes.
    #[must_use]
    pub fn nodes(&self) -> &[SessionNode] {
        &self.nodes
    }

    /// Returns the entries along the current branch (root to head).
    #[must_use]
    pub fn current_branch(&self) -> Vec<&SessionEntry> {
        self.branch_from(self.head)
    }

    /// Returns the entries along the branch ending at `head` (root to
    /// `head`). When `head` is `None` or unknown, returns an empty
    /// slice. Use this to assemble a prompt for a *specific*
    /// conversation that may not coincide with the tree's write
    /// cursor — multiple side conversations can interleave their
    /// tree mutations, so `tree.head()` is unreliable for that
    /// purpose.
    #[must_use]
    pub fn branch_from(&self, head: Option<NodeId>) -> Vec<&SessionEntry> {
        self.branch_node_ids_from(head)
            .into_iter()
            .filter_map(|id| self.node(id).map(|node| &node.entry))
            .collect()
    }

    /// Returns foreground tool calls on `head`'s branch that still lack a
    /// terminal provider result.
    ///
    /// Results are ordered by assistant response and by the model's original
    /// tool-call order. Calls that already have a terminal result in a
    /// partially completed parallel round are omitted. Backgrounded tool
    /// calls are out of scope because their foreground is already closed by
    /// a synthetic provider result.
    #[must_use]
    pub fn unresolved_foreground_tool_calls_from(
        &self,
        head: Option<NodeId>,
    ) -> Vec<&ToolCallItem> {
        let mut calls = Vec::new();
        for node_id in self.branch_node_ids_from(head) {
            let Some(round) = self.pending_tool_rounds.get(&node_id) else {
                continue;
            };
            let Some(SessionEntry::AssistantResponse { output_items, .. }) =
                self.node(node_id).map(|node| &node.entry)
            else {
                continue;
            };
            for call_id in &round.call_order {
                if round.terminal_results.contains_key(call_id) {
                    continue;
                }
                if let Some(call) = output_items.iter().find_map(|item| match item {
                    ContextItem::ToolCall(call) if &call.call_id == call_id => Some(call),
                    _ => None,
                }) {
                    calls.push(call);
                }
            }
        }
        calls
    }

    /// Returns backgrounded tool calls on `head`'s branch and any durable
    /// background completion recorded for them.
    ///
    /// The provider-visible placeholder is stored as a `ProviderToolResult`
    /// with [`ToolResultKind::BackgroundPlaceholder`]. The later real outcome
    /// is stored separately as `ToolBackgroundResult` or
    /// `ToolBackgroundError` and does not fold into the prompt tree, so
    /// callers must pass the durable event log alongside the tree. Completed
    /// calls are returned in durable completion-event order; unfinished
    /// placeholders follow in provider-placeholder order.
    #[must_use]
    pub fn background_tool_calls_from(
        &self,
        head: Option<NodeId>,
        events: &[PersistedSessionEvent],
    ) -> Vec<BackgroundToolCallState> {
        let branch_call_ids = self.tool_call_ids_from_branch(head);
        if branch_call_ids.is_empty() {
            return Vec::new();
        }

        let mut placeholder_order = Vec::new();
        let mut completion_order = Vec::new();
        let mut completion_order_seen = HashSet::new();
        let mut states = HashMap::new();
        let mut completions = HashMap::new();
        for entry in events {
            match &entry.event {
                Event::ProviderToolResult(result) => {
                    if result.kind != ToolResultKind::BackgroundPlaceholder
                        || !branch_call_ids.contains(&result.call_id)
                    {
                        continue;
                    }
                    if states.contains_key(&result.call_id) {
                        continue;
                    }
                    placeholder_order.push(result.call_id.clone());
                    states.insert(
                        result.call_id.clone(),
                        BackgroundToolCallState {
                            placeholder: BackgroundToolPlaceholder {
                                call_id: result.call_id.clone(),
                                tool_name: result.tool_name.clone(),
                                tool_type: result.tool_type,
                                originator: result.originator.clone(),
                            },
                            completion: completions.get(&result.call_id).cloned(),
                        },
                    );
                }
                Event::ToolBackgroundResult(result) => {
                    if !branch_call_ids.contains(&result.call_id) {
                        continue;
                    }
                    let completion = BackgroundToolCompletion::Result(result.clone());
                    completions.insert(result.call_id.clone(), completion.clone());
                    if completion_order_seen.insert(result.call_id.clone()) {
                        completion_order.push(result.call_id.clone());
                    }
                    if let Some(state) = states.get_mut(&result.call_id) {
                        state.completion = Some(completion);
                    }
                }
                Event::ToolBackgroundError(error) => {
                    if !branch_call_ids.contains(&error.call_id) {
                        continue;
                    }
                    let completion = BackgroundToolCompletion::Error(error.clone());
                    completions.insert(error.call_id.clone(), completion.clone());
                    if completion_order_seen.insert(error.call_id.clone()) {
                        completion_order.push(error.call_id.clone());
                    }
                    if let Some(state) = states.get_mut(&error.call_id) {
                        state.completion = Some(completion);
                    }
                }
                _ => {}
            }
        }

        let mut ordered = Vec::new();
        for call_id in completion_order {
            if let Some(state) = states.remove(&call_id) {
                ordered.push(state);
            }
        }
        for call_id in placeholder_order {
            if let Some(state) = states.remove(&call_id) {
                ordered.push(state);
            }
        }
        ordered
    }

    /// Returns background placeholders on `head`'s branch that lack a real
    /// background result or error in the durable event log.
    #[must_use]
    pub fn unresolved_background_tool_calls_from(
        &self,
        head: Option<NodeId>,
        events: &[PersistedSessionEvent],
    ) -> Vec<BackgroundToolPlaceholder> {
        self.background_tool_calls_from(head, events)
            .into_iter()
            .filter(|state| state.completion.is_none())
            .map(|state| state.placeholder)
            .collect()
    }

    fn tool_call_ids_from_branch(&self, head: Option<NodeId>) -> HashSet<ToolCallId> {
        let mut call_ids = HashSet::new();
        for node_id in self.branch_node_ids_from(head) {
            let Some(SessionEntry::AssistantResponse { output_items, .. }) =
                self.node(node_id).map(|node| &node.entry)
            else {
                continue;
            };
            for item in output_items {
                if let ContextItem::ToolCall(call) = item {
                    call_ids.insert(call.call_id.clone());
                }
            }
        }
        call_ids
    }

    fn branch_node_ids_from(&self, head: Option<NodeId>) -> Vec<NodeId> {
        let mut path = Vec::new();
        let mut current = head;
        while let Some(id) = current {
            if let Some(node) = self.nodes.get(id.get() as usize) {
                path.push(id);
                current = node.parent_id;
            } else {
                break;
            }
        }
        path.reverse();
        path
    }

    /// Returns the direct children of a node.
    #[must_use]
    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|n| n.parent_id == Some(id))
            .map(|n| n.id)
            .collect()
    }

    /// Returns the leaves of the tree — every node that has no
    /// children. Each leaf is the tip of one branch the user can
    /// resume by setting the head to it. Order matches insertion
    /// order (NodeId-ascending).
    #[must_use]
    pub fn leaves(&self) -> Vec<NodeId> {
        use std::collections::HashSet;
        let parents: HashSet<NodeId> = self.nodes.iter().filter_map(|n| n.parent_id).collect();
        self.nodes
            .iter()
            .map(|n| n.id)
            .filter(|id| !parents.contains(id))
            .collect()
    }

    fn append_node_at(&mut self, parent: Option<NodeId>, entry: SessionEntry) -> NodeId {
        let id = NodeId::new(self.nodes.len() as u64);
        self.nodes.push(SessionNode {
            id,
            parent_id: parent,
            entry,
        });
        self.head = Some(id);
        id
    }

    /// Folds a slice of durable session events into a fresh tree.
    ///
    /// Replay is purely positional: NodeIds are assigned by insertion
    /// order, so the same event slice always yields the same tree.
    /// Events that don't directly produce a session entry (lifecycle
    /// chatter, harness info, etc.) are ignored.
    #[must_use]
    pub fn from_events(session_id: SessionId, events: &[PersistedSessionEvent]) -> Self {
        let mut tree = Self {
            session_id,
            nodes: Vec::new(),
            head: None,
            next_event_id: LogEventId::new(FIRST_EVENT_ID),
            pending_tool_rounds: HashMap::new(),
            tool_call_rounds: HashMap::new(),
        };
        for entry in events {
            // Persisted records store the inner `Option<NodeId>` only
            // (serde collapses `Some(None)` to `None`), so `None` in
            // the durable record always means "inherit head" on
            // replay. Sessions that branch via explicit-root publishes
            // (e.g. fresh sub-agent contexts) lose that distinction
            // across daemon restarts — acceptable today since side
            // conversations are not resumed across restarts.
            tree.apply_event_at(entry.parent_node_id.map(Some), &entry.event);
            tree.next_event_id = LogEventId::new(entry.id.get() + 1);
        }
        tree
    }

    /// Returns the id the next durable event appended to this
    /// session's log should receive. Maintained incrementally by
    /// `SessionStore::append_session_event_at`; on replay,
    /// initialised from the highest persisted event id.
    #[must_use]
    pub fn next_event_id(&self) -> LogEventId {
        self.next_event_id
    }

    /// Bumps the cached next-event-id after a successful append.
    /// Crate-internal — only the session store mutates this.
    pub(crate) fn advance_next_event_id(&mut self) {
        self.next_event_id = LogEventId::new(self.next_event_id.get() + 1);
    }

    /// Incrementally apply one durable event to the tree. Mirrors the
    /// fold rules of [`SessionTree::from_events`]. Tree-folding events
    /// are parented at the current `head`; for callers that need to
    /// fold an event onto a *specific* branch (without first emitting
    /// a `UiNavigateTree` to bounce `head` there), use
    /// [`SessionTree::apply_event_at`].
    pub fn apply_event(&mut self, event: &Event) {
        self.apply_event_at(None, event);
    }

    /// Like [`SessionTree::apply_event`] but parents the produced
    /// node under an explicit fold parent. The `Option<Option<NodeId>>`
    /// tri-state distinguishes:
    /// * `None` — no caller-supplied parent; inherit the tree's current `head`
    ///   (legacy behaviour, used by transient publishes and by replay of older
    ///   persisted records).
    /// * `Some(None)` — fold the produced node at the *root* (no parent). Used
    ///   to start a fresh branch (e.g. a sub-agent's first turn) without
    ///   inheriting the tree's current cursor.
    /// * `Some(Some(id))` — fold under the given node.
    ///
    /// Returns the id of the node this event produced, or `None` for
    /// events that don't fold (transient lifecycle chatter, an
    /// `ProviderResponseFinished` carrying only tool calls, a
    /// `UiNavigateTree`, etc.). Callers tracking a per-conversation
    /// branch cursor must advance it only when this returns `Some` —
    /// `tree.head()` is the *global* write cursor, so syncing blindly
    /// to it after a non-folding event would steal whichever other
    /// conversation's node the cursor last visited.
    pub fn apply_event_at(
        &mut self,
        parent: Option<Option<NodeId>>,
        event: &Event,
    ) -> Option<NodeId> {
        let parent = match parent {
            None => self.head,
            Some(explicit) => explicit,
        };
        match event {
            Event::UiPromptSubmitted(prompt) => Some(self.append_node_at(
                parent,
                SessionEntry::UserInput {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: prompt.text.clone(),
                        }],
                        phase: None,
                    })],
                },
            )),
            Event::SessionUserMessageInjected(injected) => Some(self.append_node_at(
                parent,
                SessionEntry::UserInput {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: injected.text.clone(),
                        }],
                        phase: None,
                    })],
                },
            )),
            Event::SessionPromptSteered(steered) => Some(self.append_node_at(
                parent,
                SessionEntry::UserInput {
                    items: vec![ContextItem::Message(MessageItem {
                        role: ContextRole::User,
                        content: vec![ContentPart::Text {
                            text: steered.text.clone(),
                        }],
                        phase: None,
                    })],
                },
            )),
            Event::SessionCompacted(compacted) => Some(self.append_node_at(
                parent,
                SessionEntry::Compaction {
                    replacement_window: compacted.replacement_window.clone(),
                },
            )),
            Event::ProviderResponseFinished(response) => {
                let node_id = self.append_node_at(
                    parent,
                    SessionEntry::AssistantResponse {
                        provider_response_id: response.provider_response_id.clone(),
                        backend: response.backend.clone(),
                        output_items: response.output_items.clone(),
                        usage: response.usage.clone(),
                    },
                );
                let mut call_order = Vec::new();
                let mut seen = HashSet::new();
                for item in &response.output_items {
                    if let ContextItem::ToolCall(call) = item {
                        assert!(
                            seen.insert(call.call_id.clone()),
                            "duplicate tool call id in agent response: {}",
                            call.call_id
                        );
                        assert!(
                            !self.tool_call_rounds.contains_key(&call.call_id),
                            "tool call id reused while a round is open: {}",
                            call.call_id
                        );
                        call_order.push(call.call_id.clone());
                    }
                }
                if !call_order.is_empty() {
                    for call_id in &call_order {
                        self.tool_call_rounds.insert(call_id.clone(), node_id);
                    }
                    self.pending_tool_rounds.insert(
                        node_id,
                        PendingToolRound {
                            assistant_node_id: node_id,
                            call_order,
                            terminal_results: HashMap::new(),
                        },
                    );
                }
                Some(node_id)
            }
            Event::ToolRequest(_)
            | Event::ToolStarted(_)
            | Event::ToolRejected(_)
            | Event::ToolResult(_)
            | Event::ToolError(_) => None,
            Event::ProviderToolResult(result) => self.record_terminal_tool_result(ToolResultItem {
                call_id: result.call_id.clone(),
                tool_type: result.tool_type,
                status: ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&result.result),
            }),
            Event::ProviderToolError(error) => self.record_terminal_tool_result(ToolResultItem {
                call_id: error.call_id.clone(),
                tool_type: error.tool_type,
                status: ToolResultStatus::Error {
                    message: error.message.clone(),
                },
                output: tau_proto::ToolResponse::from_cbor(
                    error
                        .details
                        .as_ref()
                        .unwrap_or(&tau_proto::CborValue::Null),
                ),
            }),
            Event::ToolCancelled(cancelled) => self.record_terminal_tool_result(ToolResultItem {
                call_id: cancelled.call_id.clone(),
                tool_type: cancelled.tool_type,
                status: ToolResultStatus::Cancelled {
                    reason: "cancelled".to_owned(),
                },
                output: tau_proto::ToolResponse::from_cbor(&tau_proto::CborValue::Null),
            }),
            Event::UiNavigateTree(req) => {
                let target = NodeId::new(req.node_id);
                if (target.get() as usize) < self.nodes.len() {
                    self.head = Some(target);
                }
                None
            }
            _ => None,
        }
    }

    /// Validate an event against the current transcript fold state before
    /// appending it to the durable log.
    pub fn validate_event(&self, event: &Event) -> Result<(), SessionEventValidationError> {
        match event {
            Event::ProviderResponseFinished(response) => {
                let mut seen = HashSet::new();
                for item in &response.output_items {
                    let ContextItem::ToolCall(call) = item else {
                        continue;
                    };
                    if call.call_id.as_str().is_empty() {
                        return Err(SessionEventValidationError::new(
                            "agent response contained an empty tool call id",
                        ));
                    }
                    if !seen.insert(call.call_id.clone()) {
                        return Err(SessionEventValidationError::new(format!(
                            "agent response contained duplicate tool call id: {}",
                            call.call_id
                        )));
                    }
                    if self.tool_call_rounds.contains_key(&call.call_id) {
                        return Err(SessionEventValidationError::new(format!(
                            "agent response reused open tool call id: {}",
                            call.call_id
                        )));
                    }
                }
                Ok(())
            }
            Event::ProviderToolResult(result) => {
                self.validate_terminal_tool_result(&result.call_id)
            }
            Event::ProviderToolError(error) => self.validate_terminal_tool_result(&error.call_id),
            Event::ToolCancelled(cancelled) => {
                self.validate_terminal_tool_result(&cancelled.call_id)
            }
            _ => Ok(()),
        }
    }

    fn record_terminal_tool_result(&mut self, item: ToolResultItem) -> Option<NodeId> {
        let Some(assistant_node_id) = self.tool_call_rounds.get(&item.call_id).copied() else {
            panic!(
                "terminal tool result for unknown or already-closed call_id: {}",
                item.call_id
            );
        };
        let Some(round) = self.pending_tool_rounds.get_mut(&assistant_node_id) else {
            panic!(
                "tool call mapped to missing pending round: {}",
                item.call_id
            );
        };
        round.terminal_results.insert(item.call_id.clone(), item);
        if round.terminal_results.len() != round.call_order.len() {
            return None;
        }

        let round = self
            .pending_tool_rounds
            .remove(&assistant_node_id)
            .expect("pending round should exist when terminal");
        for call_id in &round.call_order {
            self.tool_call_rounds.remove(call_id);
        }
        let items = round
            .call_order
            .iter()
            .map(|call_id| {
                round
                    .terminal_results
                    .get(call_id)
                    .cloned()
                    .expect("terminal round missing tool result")
            })
            .collect();
        Some(self.append_node_at(
            Some(round.assistant_node_id),
            SessionEntry::ToolResults { items },
        ))
    }

    fn validate_terminal_tool_result(
        &self,
        call_id: &ToolCallId,
    ) -> Result<(), SessionEventValidationError> {
        let Some(assistant_node_id) = self.tool_call_rounds.get(call_id) else {
            return Err(SessionEventValidationError::new(format!(
                "terminal tool result for unknown or already-closed call_id: {call_id}"
            )));
        };
        let Some(round) = self.pending_tool_rounds.get(assistant_node_id) else {
            return Err(SessionEventValidationError::new(format!(
                "tool call mapped to missing pending round: {call_id}"
            )));
        };
        if round.terminal_results.contains_key(call_id) {
            return Err(SessionEventValidationError::new(format!(
                "duplicate terminal tool result for call_id: {call_id}"
            )));
        }
        Ok(())
    }
}

/// One durable session-scoped protocol event.
///
/// `parent_node_id` is the explicit fold parent that was passed to
/// `SessionStore::append_session_event_at` at write time. Carrying
/// it on the persisted record (rather than on the wire) preserves
/// cross-conversation branching across replay without requiring the
/// publisher-side `UiNavigateTree` head-bouncing dance the harness
/// used to do. Older records without this field deserialize as
/// `None` and replay against the live `tree.head()` — matching the
/// legacy single-cursor fold and so back-compatible.
///
/// Lossy round-trip on the tri-state: the in-memory API distinguishes
/// `None` (inherit head) from `Some(None)` (explicit-root, e.g. a
/// fresh sub-agent context), but only the inner `Option<NodeId>` is
/// persisted — `Some(None)` collapses to `None` on disk. On replay,
/// both look like "inherit head", so sessions branched via
/// explicit-root publishes lose that distinction across daemon
/// restarts.
//
// TODO(sub-agent-resume): when sub-agent contexts need to be resumed
// across restarts, persist the tri-state explicitly (e.g. an enum
// `{Inherit, Root, Under(NodeId)}`) instead of `Option<NodeId>`. See
// also `SessionTree::from_events`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedSessionEvent {
    pub id: LogEventId,
    pub source: Option<ConnectionId>,
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_node_id: Option<NodeId>,
    /// Wall-clock micros since UNIX epoch when the event was
    /// appended, matching the value carried on the wire `LogEvent`
    /// envelope and stamped in
    /// [`crate::SessionStore::append_session_event_at`]. `UnixMicros(0)` on
    /// records written before this field existed (deserialized via
    /// `#[serde(default)]`). Used for offline inspection — inter-turn
    /// timing, RPM bursts, cache-miss correlation — never for replay
    /// semantics.
    #[serde(default)]
    pub recorded_at: UnixMicros,
}

/// Per-session sidecar metadata at `<sessions_dir>/<session_id>/meta.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Working directory at the time of session creation.
    pub cwd: Option<PathBuf>,
    /// Unix epoch seconds when the session was first created.
    pub created_at: u64,
    /// Unix epoch seconds of the most recent append.
    pub last_touched: u64,
    /// Preview of the latest user-authored prompt, used by the resume picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_user_prompt_preview: Option<String>,
}
