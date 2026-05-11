//! Tree-structured session history types and the persisted-event
//! record they are derived from.
//!
//! The on-disk source of truth is the per-session protocol-event log
//! ([`PersistedSessionEvent`] / `events.cbor`); the in-memory
//! [`SessionTree`] is built from it via [`SessionTree::from_events`]
//! and kept in sync incrementally by [`SessionTree::apply_event`]. No
//! other API mutates the tree, so the on-disk log and the cached
//! view cannot drift.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tau_proto::{ConnectionId, Event, LogEventId, SessionId, ToolCallId, ToolName};

/// Default starting `LogEventId` for a tree with no events.
const FIRST_EVENT_ID: u64 = 0;

/// One persisted chat or tool activity entry belonging to a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SessionEntry {
    UserMessage {
        text: String,
    },
    AgentMessage {
        text: String,
        /// Provider-supplied reasoning summary captured during the
        /// turn, if any. Persisted alongside the response so resume
        /// can re-render it; intentionally excluded from prompt
        /// replay (see harness `assemble_conversation`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    ToolActivity(ToolActivityRecord),
}

/// One persisted tool activity record associated with a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolActivityRecord {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub outcome: ToolActivityOutcome,
}

/// The persisted outcome of one tool activity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToolActivityOutcome {
    Requested {
        arguments: tau_proto::CborValue,
    },
    Result {
        result: tau_proto::CborValue,
    },
    Error {
        message: String,
        details: Option<tau_proto::CborValue>,
    },
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
        self.nodes.get(id.0 as usize)
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
        let mut path = Vec::new();
        let mut current = head;
        while let Some(id) = current {
            if let Some(node) = self.nodes.get(id.0 as usize) {
                path.push(&node.entry);
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
        let id = NodeId(self.nodes.len() as u64);
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
    /// [`SessionStore::append_session_event_at`]; on replay,
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
    /// `AgentResponseFinished` carrying only tool calls, a
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
                SessionEntry::UserMessage {
                    text: prompt.text.clone(),
                },
            )),
            Event::SessionUserMessageInjected(injected) => Some(self.append_node_at(
                parent,
                SessionEntry::UserMessage {
                    text: injected.text.clone(),
                },
            )),
            Event::SessionPromptSteered(steered) => Some(self.append_node_at(
                parent,
                SessionEntry::UserMessage {
                    text: steered.text.clone(),
                },
            )),
            Event::AgentResponseFinished(response) => response.text.as_ref().map(|text| {
                self.append_node_at(
                    parent,
                    SessionEntry::AgentMessage {
                        text: text.clone(),
                        thinking: response.thinking.clone(),
                    },
                )
            }),
            Event::ToolRequest(request) => Some(self.append_node_at(
                parent,
                SessionEntry::ToolActivity(ToolActivityRecord {
                    call_id: request.call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    outcome: ToolActivityOutcome::Requested {
                        arguments: request.arguments.clone(),
                    },
                }),
            )),
            Event::ToolResult(result) => Some(self.append_node_at(
                parent,
                SessionEntry::ToolActivity(ToolActivityRecord {
                    call_id: result.call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    outcome: ToolActivityOutcome::Result {
                        result: result.result.clone(),
                    },
                }),
            )),
            Event::ToolError(error) => Some(self.append_node_at(
                parent,
                SessionEntry::ToolActivity(ToolActivityRecord {
                    call_id: error.call_id.clone(),
                    tool_name: error.tool_name.clone(),
                    outcome: ToolActivityOutcome::Error {
                        message: error.message.clone(),
                        details: error.details.clone(),
                    },
                }),
            )),
            Event::UiNavigateTree(req) => {
                let target = NodeId(req.node_id);
                if (target.0 as usize) < self.nodes.len() {
                    self.head = Some(target);
                }
                None
            }
            _ => None,
        }
    }
}

/// One durable session-scoped protocol event.
///
/// `parent_node_id` is the explicit fold parent that was passed to
/// [`SessionStore::append_session_event_at`] at write time. Carrying
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
}

/// Per-session sidecar metadata at `<state_dir>/<session_id>/meta.json`.
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
