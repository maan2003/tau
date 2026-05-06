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

/// Unique identifier for a node in the session tree.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct NodeId(pub u64);

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

    fn append_node(&mut self, entry: SessionEntry) -> NodeId {
        let id = NodeId(self.nodes.len() as u64);
        self.nodes.push(SessionNode {
            id,
            parent_id: self.head,
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
        };
        for entry in events {
            tree.apply_event(&entry.event);
        }
        tree
    }

    /// Incrementally apply one durable event to the tree. Mirrors the
    /// fold rules of [`SessionTree::from_events`].
    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::UiPromptSubmitted(prompt) => {
                self.append_node(SessionEntry::UserMessage {
                    text: prompt.text.clone(),
                });
            }
            Event::SessionUserMessageInjected(injected) => {
                self.append_node(SessionEntry::UserMessage {
                    text: injected.text.clone(),
                });
            }
            Event::AgentResponseFinished(response) => {
                if let Some(text) = response.text.as_ref() {
                    self.append_node(SessionEntry::AgentMessage {
                        text: text.clone(),
                        thinking: response.thinking.clone(),
                    });
                }
            }
            Event::ToolRequest(request) => {
                self.append_node(SessionEntry::ToolActivity(ToolActivityRecord {
                    call_id: request.call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    outcome: ToolActivityOutcome::Requested {
                        arguments: request.arguments.clone(),
                    },
                }));
            }
            Event::ToolResult(result) => {
                self.append_node(SessionEntry::ToolActivity(ToolActivityRecord {
                    call_id: result.call_id.clone(),
                    tool_name: result.tool_name.clone(),
                    outcome: ToolActivityOutcome::Result {
                        result: result.result.clone(),
                    },
                }));
            }
            Event::ToolError(error) => {
                self.append_node(SessionEntry::ToolActivity(ToolActivityRecord {
                    call_id: error.call_id.clone(),
                    tool_name: error.tool_name.clone(),
                    outcome: ToolActivityOutcome::Error {
                        message: error.message.clone(),
                        details: error.details.clone(),
                    },
                }));
            }
            Event::UiNavigateTree(req) => {
                let target = NodeId(req.node_id);
                if (target.0 as usize) < self.nodes.len() {
                    self.head = Some(target);
                }
            }
            _ => {}
        }
    }
}

/// One durable session-scoped protocol event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedSessionEvent {
    pub id: LogEventId,
    pub source: Option<ConnectionId>,
    pub event: Event,
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
}
