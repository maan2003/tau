//! Event names and subscription selectors.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// First segment of a dotted event name.
///
/// The well-known categories are enumerated so that subscription
/// policies, routing, and other category-level logic can branch on a
/// closed set. Unknown categories — e.g. from a future extension that
/// invents its own family — round-trip through [`EventCategory::Other`]
/// without losing fidelity.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum EventCategory {
    /// Tool execution events.
    Tool,
    /// Extension-provided UI action events.
    Action,
    /// Harness-owned side/sub-agent command events.
    Agent,
    /// Extension lifecycle and publication events.
    Extension,
    /// Provider backend events.
    Provider,
    /// Harness status and configuration events.
    Harness,
    /// User-interface request events.
    Ui,
    /// Shell command lifecycle events.
    Shell,
    /// Session lifecycle and durable transcript events.
    Session,
    /// Terminal-output side effects directed at the UI: escape
    /// sequences the UI should write straight through to its
    /// terminal (notifications, OSC user-vars, etc.).
    Term,
    /// Any category we don't recognize, kept verbatim.
    Other(String),
}

impl EventCategory {
    /// The wire string for this category (the part before the first
    /// dot in the dotted name).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Tool => "tool",
            Self::Action => "action",
            Self::Agent => "agent",
            Self::Extension => "extension",
            Self::Provider => "provider",
            Self::Harness => "harness",
            Self::Ui => "ui",
            Self::Shell => "shell",
            Self::Session => "session",
            Self::Term => "term",
            Self::Other(s) => s.as_str(),
        }
    }

    /// Parse a category string. Always succeeds; unknown strings
    /// become [`EventCategory::Other`].
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "tool" => Self::Tool,
            "action" => Self::Action,
            "agent" => Self::Agent,
            "extension" => Self::Extension,
            "provider" => Self::Provider,
            "harness" => Self::Harness,
            "ui" => Self::Ui,
            "shell" => Self::Shell,
            "session" => Self::Session,
            "term" => Self::Term,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl fmt::Display for EventCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Second segment of a dotted event name.
///
/// Open-ended: the wire format permits arbitrary identifiers after
/// the first dot, so this is just a string newtype rather than a
/// closed enum. Borrowed `&'static str` for the well-known constants
/// declared on [`EventName`]; owned `String` for anything decoded
/// from the wire or constructed at runtime.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EventCall(std::borrow::Cow<'static, str>);

impl EventCall {
    /// Create an event call from a static string.
    pub const fn from_static(s: &'static str) -> Self {
        Self(std::borrow::Cow::Borrowed(s))
    }

    /// Create an event call from owned or borrowed text.
    pub fn new(s: impl Into<String>) -> Self {
        Self(std::borrow::Cow::Owned(s.into()))
    }

    /// Borrow the call segment as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EventCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&'static str> for EventCall {
    fn from(s: &'static str) -> Self {
        Self::from_static(s)
    }
}

impl From<String> for EventCall {
    fn from(s: String) -> Self {
        Self(std::borrow::Cow::Owned(s))
    }
}

/// A dotted event name, split into a category and a call.
///
/// Wire format is `"<category>.<call>"`; serde and `Display` use that
/// form. The well-known protocol events are exposed as `pub const`
/// values directly on this type (`EventName::TOOL_REGISTER`, etc.) so
/// match-arm-style call sites keep their compactness while gaining
/// a typed `category` to branch on.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct EventName {
    /// First segment of the dotted event name.
    pub category: EventCategory,
    /// Second segment of the dotted event name.
    pub call: EventCall,
}

impl EventName {
    /// Create an event name from a category and static call segment.
    pub const fn from_static(category: EventCategory, call: &'static str) -> Self {
        Self {
            category,
            call: EventCall::from_static(call),
        }
    }

    /// Create an event name from a category and call segment.
    pub fn new(category: EventCategory, call: impl Into<EventCall>) -> Self {
        Self {
            category,
            call: call.into(),
        }
    }

    /// True iff the dotted form `"<category>.<call>"` starts with
    /// `prefix`. Avoids allocating; handles category-only and
    /// across-the-dot prefixes correctly.
    #[must_use]
    pub fn matches_prefix(&self, prefix: &str) -> bool {
        let cat = self.category.as_str();
        if prefix.len() <= cat.len() {
            return cat.starts_with(prefix);
        }
        // prefix extends past the category — it must include the dot.
        if !prefix.starts_with(cat) {
            return false;
        }
        match prefix.as_bytes().get(cat.len()) {
            Some(&b'.') => self.call.as_str().starts_with(&prefix[cat.len() + 1..]),
            _ => false,
        }
    }

    // -- Well-known event names ----------------------------------------

    pub const TOOL_REGISTER: Self = Self::from_static(EventCategory::Tool, "register");
    pub const TOOL_UNREGISTER: Self = Self::from_static(EventCategory::Tool, "unregister");
    pub const TOOL_REQUEST: Self = Self::from_static(EventCategory::Tool, "request");
    pub const TOOL_STARTED: Self = Self::from_static(EventCategory::Tool, "started");
    pub const TOOL_REJECTED: Self = Self::from_static(EventCategory::Tool, "rejected");
    pub const TOOL_RESULT: Self = Self::from_static(EventCategory::Tool, "result");
    pub const TOOL_ERROR: Self = Self::from_static(EventCategory::Tool, "error");
    pub const TOOL_BACKGROUND_RESULT: Self =
        Self::from_static(EventCategory::Tool, "background_result");
    pub const TOOL_BACKGROUND_ERROR: Self =
        Self::from_static(EventCategory::Tool, "background_error");
    pub const TOOL_PROGRESS: Self = Self::from_static(EventCategory::Tool, "progress");
    pub const TOOL_CANCEL_REQUEST: Self = Self::from_static(EventCategory::Tool, "cancel_request");
    pub const TOOL_CANCELLED: Self = Self::from_static(EventCategory::Tool, "cancelled");
    pub const TOOL_DELEGATE_PROGRESS: Self =
        Self::from_static(EventCategory::Tool, "delegate_progress");

    pub const ACTION_SCHEMA_PUBLISHED: Self =
        Self::from_static(EventCategory::Action, "schema_published");
    pub const ACTION_INVOKE: Self = Self::from_static(EventCategory::Action, "invoke");
    pub const ACTION_RESULT: Self = Self::from_static(EventCategory::Action, "result");
    pub const ACTION_ERROR: Self = Self::from_static(EventCategory::Action, "error");

    pub const EXTENSION_STARTING: Self = Self::from_static(EventCategory::Extension, "starting");
    pub const EXTENSION_READY: Self = Self::from_static(EventCategory::Extension, "ready");
    pub const EXTENSION_EXITED: Self = Self::from_static(EventCategory::Extension, "exited");
    pub const EXTENSION_RESTARTING: Self =
        Self::from_static(EventCategory::Extension, "restarting");
    pub const EXTENSION_SKILL_AVAILABLE: Self =
        Self::from_static(EventCategory::Extension, "skill_available");
    pub const EXTENSION_AGENTS_MD_AVAILABLE: Self =
        Self::from_static(EventCategory::Extension, "agents_md_available");
    pub const EXTENSION_CONTEXT_READY: Self =
        Self::from_static(EventCategory::Extension, "context_ready");
    pub const EXTENSION_SESSION_CONTEXT_PUBLISH: Self =
        Self::from_static(EventCategory::Extension, "session_context_publish");
    pub const EXTENSION_PROMPT_FRAGMENT_PUBLISH: Self =
        Self::from_static(EventCategory::Extension, "prompt_fragment_publish");
    pub const AGENT_START_REQUEST: Self = Self::from_static(EventCategory::Agent, "start_request");
    pub const AGENT_START_ACCEPTED: Self =
        Self::from_static(EventCategory::Agent, "start_accepted");
    pub const AGENT_START_RESULT: Self = Self::from_static(EventCategory::Agent, "start_result");
    pub const AGENT_MESSAGE: Self = Self::from_static(EventCategory::Agent, "message");
    pub const PROVIDER_MODELS_UPDATED: Self =
        Self::from_static(EventCategory::Provider, "models_updated");
    pub const PROVIDER_TOOL_RESULT: Self =
        Self::from_static(EventCategory::Provider, "tool_result");
    pub const PROVIDER_TOOL_ERROR: Self = Self::from_static(EventCategory::Provider, "tool_error");
    pub const PROVIDER_PROMPT_SUBMITTED: Self =
        Self::from_static(EventCategory::Provider, "prompt_submitted");
    pub const PROVIDER_RESPONSE_UPDATED: Self =
        Self::from_static(EventCategory::Provider, "response_updated");
    pub const PROVIDER_RESPONSE_FINISHED: Self =
        Self::from_static(EventCategory::Provider, "response_finished");
    pub const PROVIDER_CACHE_MISS_DIAGNOSTIC: Self =
        Self::from_static(EventCategory::Provider, "cache_miss_diagnostic");

    pub const HARNESS_INFO: Self = Self::from_static(EventCategory::Harness, "info");
    pub const HARNESS_SESSION_DIR: Self = Self::from_static(EventCategory::Harness, "session_dir");
    pub const HARNESS_UI_DIR: Self = Self::from_static(EventCategory::Harness, "ui_dir");
    pub const HARNESS_MODELS_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "models_available");
    pub const HARNESS_ROLES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "roles_available");
    pub const HARNESS_ROLE_SELECTED: Self =
        Self::from_static(EventCategory::Harness, "role_selected");
    pub const HARNESS_CONTEXT_USAGE_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "context_usage_changed");
    pub const HARNESS_EFFORTS_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "efforts_available");
    pub const HARNESS_VERBOSITIES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "verbosities_available");
    pub const HARNESS_THINKING_SUMMARIES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "thinking_summaries_available");

    pub const UI_PROMPT_SUBMITTED: Self = Self::from_static(EventCategory::Ui, "prompt_submitted");
    pub const UI_ROLE_SELECT: Self = Self::from_static(EventCategory::Ui, "role_select");
    pub const UI_ROLE_UPDATE: Self = Self::from_static(EventCategory::Ui, "role_update");
    pub const UI_DETACH_REQUEST: Self = Self::from_static(EventCategory::Ui, "detach_request");
    pub const UI_SHELL_COMMAND: Self = Self::from_static(EventCategory::Ui, "shell_command");
    pub const UI_SWITCH_SESSION: Self = Self::from_static(EventCategory::Ui, "switch_session");
    pub const UI_NEW_AGENT: Self = Self::from_static(EventCategory::Ui, "new_agent");
    pub const UI_AGENT_STATE_REQUEST: Self =
        Self::from_static(EventCategory::Ui, "agent_state_request");
    pub const UI_TREE_REQUEST: Self = Self::from_static(EventCategory::Ui, "tree_request");
    pub const UI_NAVIGATE_TREE: Self = Self::from_static(EventCategory::Ui, "navigate_tree");
    pub const UI_COMPACT_REQUEST: Self = Self::from_static(EventCategory::Ui, "compact_request");
    pub const UI_PROMPT_DRAFT: Self = Self::from_static(EventCategory::Ui, "prompt_draft");
    pub const UI_CANCEL_PROMPT: Self = Self::from_static(EventCategory::Ui, "cancel_prompt");
    pub const UI_RECALL_QUEUED_PROMPT: Self =
        Self::from_static(EventCategory::Ui, "recall_queued_prompt");

    pub const TERM_OSC1337_SET_USER_VAR: Self =
        Self::from_static(EventCategory::Term, "osc1337_set_user_var");

    pub const SHELL_COMMAND_PROGRESS: Self =
        Self::from_static(EventCategory::Shell, "command_progress");
    pub const SHELL_COMMAND_FINISHED: Self =
        Self::from_static(EventCategory::Shell, "command_finished");

    pub const SESSION_PROMPT_QUEUED: Self =
        Self::from_static(EventCategory::Session, "prompt_queued");
    pub const SESSION_PROMPT_RECALLED: Self =
        Self::from_static(EventCategory::Session, "prompt_recalled");
    pub const SESSION_PROMPT_STEERED: Self =
        Self::from_static(EventCategory::Session, "prompt_steered");
    pub const SESSION_STARTED: Self = Self::from_static(EventCategory::Session, "started");
    pub const SESSION_SHUTDOWN: Self = Self::from_static(EventCategory::Session, "shutdown");
    pub const SESSION_AGENT_STATE_CHANGED: Self =
        Self::from_static(EventCategory::Session, "agent_state_changed");
    pub const SESSION_COMPACTION_STARTED: Self =
        Self::from_static(EventCategory::Session, "compaction_started");
    pub const SESSION_COMPACTION_FINISHED: Self =
        Self::from_static(EventCategory::Session, "compaction_finished");
    pub const SESSION_COMPACTED: Self = Self::from_static(EventCategory::Session, "compacted");
    pub const SESSION_COMPACTION_REQUESTED: Self =
        Self::from_static(EventCategory::Session, "compaction_requested");
    pub const SESSION_PROMPT_CREATED: Self =
        Self::from_static(EventCategory::Session, "prompt_created");
    pub const SESSION_PROMPT_TERMINATED: Self =
        Self::from_static(EventCategory::Session, "prompt_terminated");
    pub const SESSION_PROMPT_PREWARM_REQUESTED: Self =
        Self::from_static(EventCategory::Session, "prompt_prewarm_requested");
    pub const SESSION_USER_MESSAGE_INJECTED: Self =
        Self::from_static(EventCategory::Session, "user_message_injected");
}

impl fmt::Display for EventName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.category, self.call)
    }
}

impl FromStr for EventName {
    type Err = ParseEventNameError;

    /// Always succeeds for well-formed `"a.b"` input. Unknown
    /// categories survive as [`EventCategory::Other`]; unknown
    /// `call` segments survive as owned strings. Errors only on
    /// missing-dot input.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((cat, call)) = value.split_once('.') else {
            return Err(ParseEventNameError {
                invalid_name: value.to_owned(),
            });
        };
        Ok(Self {
            category: EventCategory::from_wire(cat),
            call: EventCall::new(call),
        })
    }
}

impl Serialize for EventName {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EventName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Error returned when an event-name string lacks the required
/// `<category>.<call>` shape (no dot separator).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseEventNameError {
    invalid_name: String,
}

impl ParseEventNameError {
    /// Return the invalid input string.
    #[must_use]
    pub fn invalid_name(&self) -> &str {
        &self.invalid_name
    }
}

impl fmt::Display for ParseEventNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "malformed event name (expected 'category.call'): {}",
            self.invalid_name
        )
    }
}

impl std::error::Error for ParseEventNameError {}

// ---------------------------------------------------------------------------
// Protocol participant types and selectors
// ---------------------------------------------------------------------------

/// The type of participant speaking the protocol.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// Provider backend events.
    Provider,
    /// Tool execution events.
    Tool,
    /// Extension-provided UI action events.
    Action,
    /// User-interface request events.
    Ui,
    /// Harness/core protocol participant.
    Core,
    /// External protocol participant outside the harness process.
    External,
}

/// A subscription selector used by [`crate::Subscribe`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum EventSelector {
    /// Match one exact event name.
    Exact(EventName),
    /// Match any event whose dotted name starts with this prefix.
    Prefix(String),
}
