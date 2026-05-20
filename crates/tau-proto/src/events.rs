//! Protocol event types and payloads.
//!
//! All event definitions live here so `grep events.rs` finds them.
//!
//! Events are facts — each component broadcasts what happened.
//! There are no requests or responses, only announcements.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{
    CborValue, DiffSummary, ExtensionName, ModelId, ProviderTokenUsage, SessionContextKey,
    SessionId, SessionPromptId, SkillName, ToolCallId, ToolName,
};

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_affinity_neutral(value: &i32) -> bool {
    *value == 0
}

// ---------------------------------------------------------------------------
// Event names
// ---------------------------------------------------------------------------

/// First segment of a dotted event name.
///
/// The well-known categories are enumerated so that subscription
/// policies, routing, and other category-level logic can branch on a
/// closed set. Unknown categories — e.g. from a future extension that
/// invents its own family — round-trip through [`EventCategory::Other`]
/// without losing fidelity.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum EventCategory {
    Tool,
    Extension,
    Provider,
    Harness,
    Ui,
    Shell,
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
    pub const fn from_static(s: &'static str) -> Self {
        Self(std::borrow::Cow::Borrowed(s))
    }

    pub fn new(s: impl Into<String>) -> Self {
        Self(std::borrow::Cow::Owned(s.into()))
    }

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
    pub category: EventCategory,
    pub call: EventCall,
}

impl EventName {
    pub const fn from_static(category: EventCategory, call: &'static str) -> Self {
        Self {
            category,
            call: EventCall::from_static(call),
        }
    }

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
    pub const TOOL_INVOKE: Self = Self::from_static(EventCategory::Tool, "invoke");
    pub const TOOL_RESULT: Self = Self::from_static(EventCategory::Tool, "result");
    pub const TOOL_ERROR: Self = Self::from_static(EventCategory::Tool, "error");
    pub const TOOL_BACKGROUND_RESULT: Self =
        Self::from_static(EventCategory::Tool, "background_result");
    pub const TOOL_BACKGROUND_ERROR: Self =
        Self::from_static(EventCategory::Tool, "background_error");
    pub const TOOL_PROGRESS: Self = Self::from_static(EventCategory::Tool, "progress");
    pub const TOOL_CANCEL: Self = Self::from_static(EventCategory::Tool, "cancel");
    pub const TOOL_CANCELLED: Self = Self::from_static(EventCategory::Tool, "cancelled");
    pub const TOOL_DELEGATE_PROGRESS: Self =
        Self::from_static(EventCategory::Tool, "delegate_progress");

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
    pub const EXTENSION_AGENT_QUERY: Self =
        Self::from_static(EventCategory::Extension, "agent_query");
    pub const EXTENSION_AGENT_QUERY_RESULT: Self =
        Self::from_static(EventCategory::Extension, "agent_query_result");
    pub const PROVIDER_MODELS_UPDATED: Self =
        Self::from_static(EventCategory::Provider, "models_updated");
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
    pub const HARNESS_EFFORT_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "effort_changed");
    pub const HARNESS_SERVICE_TIER_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "service_tier_changed");
    pub const HARNESS_EFFORTS_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "efforts_available");
    pub const HARNESS_VERBOSITY_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "verbosity_changed");
    pub const HARNESS_VERBOSITIES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "verbosities_available");
    pub const HARNESS_THINKING_SUMMARY_CHANGED: Self =
        Self::from_static(EventCategory::Harness, "thinking_summary_changed");
    pub const HARNESS_THINKING_SUMMARIES_AVAILABLE: Self =
        Self::from_static(EventCategory::Harness, "thinking_summaries_available");

    pub const UI_PROMPT_SUBMITTED: Self = Self::from_static(EventCategory::Ui, "prompt_submitted");
    pub const UI_ROLE_SELECT: Self = Self::from_static(EventCategory::Ui, "role_select");
    pub const UI_ROLE_UPDATE: Self = Self::from_static(EventCategory::Ui, "role_update");
    pub const UI_DETACH_REQUEST: Self = Self::from_static(EventCategory::Ui, "detach_request");
    pub const UI_SHELL_COMMAND: Self = Self::from_static(EventCategory::Ui, "shell_command");
    pub const UI_SWITCH_SESSION: Self = Self::from_static(EventCategory::Ui, "switch_session");
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
    pub const SESSION_COMPACTION_STARTED: Self =
        Self::from_static(EventCategory::Session, "compaction_started");
    pub const SESSION_COMPACTION_FINISHED: Self =
        Self::from_static(EventCategory::Session, "compaction_finished");
    pub const SESSION_COMPACTED: Self = Self::from_static(EventCategory::Session, "compacted");
    pub const SESSION_COMPACTION_REQUESTED: Self =
        Self::from_static(EventCategory::Session, "compaction_requested");
    pub const SESSION_PROMPT_CREATED: Self =
        Self::from_static(EventCategory::Session, "prompt_created");
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
    Provider,
    Tool,
    Ui,
    Core,
    External,
}

/// A subscription selector used by [`crate::Subscribe`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum EventSelector {
    Exact(EventName),
    Prefix(String),
}

/// System-prompt fragment priority. Lower numeric values render first.
///
/// The built-in system templates use coarse priority bands: priorities below
/// 100 render before generated context sections such as skills, while 100 and
/// above render afterward. Use below-100 values for role/persona instructions
/// and high values for epilogue-style context. For example, the shell extension
/// publishes the current working directory at priority 900 so it stays near the
/// end of the prompt.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct PromptPriority(u16);

impl PromptPriority {
    /// Create a prompt priority from its numeric ordering value.
    #[must_use]
    pub const fn new(v: u16) -> Self {
        Self(v)
    }

    /// Return the numeric ordering value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Text fragment inserted into a composed system prompt.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PromptContent(String);

impl PromptContent {
    /// Create prompt content from owned or borrowed text.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the prompt text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the newtype and return the owned prompt text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// Return whether the prompt text is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::Deref for PromptContent {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<String> for PromptContent {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for PromptContent {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl AsRef<str> for PromptContent {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Ordered collection of rendered prompt fragments.
pub type PromptFragments = BTreeSet<(PromptPriority, String, PromptContent)>;

/// One prompt fragment template contributed by a tool or extension.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PromptFragment {
    /// Stable fragment name, preferably namespaced by producer.
    pub name: String,
    /// Priority controlling coarse placement among fragments. Lower values
    /// render first.
    pub priority: PromptPriority,
    /// Handlebars template rendered into prompt text.
    pub template: PromptContent,
}

impl PromptFragment {
    /// Create one prompt fragment template.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        priority: PromptPriority,
        template: impl Into<PromptContent>,
    ) -> Self {
        Self {
            name: name.into(),
            priority,
            template: template.into(),
        }
    }
}

/// Interception priority. Lower numeric values run first.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct InterceptionPriority(i64);

impl InterceptionPriority {
    #[must_use]
    pub const fn new(v: i64) -> Self {
        Self(v)
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Identifier of a node in the per-session tree. Lives on the wire
/// because tree-folding events stamp their `parent_node_id` so the
/// fold doesn't have to consult a shared write cursor.
///
/// Ids are valid only against the tree that produced them. The
/// in-memory `SessionTree` uses the underlying `u64` as a positional
/// index into its node vector and assigns ids by insertion order, so
/// the same numeric id can refer to different nodes across different
/// trees. Replaying the same persisted event log yields the same ids
/// only because the fold is deterministic; an id that originated in
/// one session (or in a sub-agent's tree) is meaningless in another.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct NodeId(u64);

impl NodeId {
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Harness informational messages
// ---------------------------------------------------------------------------

/// Severity of a harness informational message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessInfoLevel {
    #[default]
    Normal,
    Important,
}

/// An informational message from the harness displayed to the user.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub message: String,
    #[serde(default)]
    pub level: HarnessInfoLevel,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDirStatus {
    #[default]
    New,
    Resumed,
}

impl SessionDirStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resumed => "resumed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessSessionDir {
    pub session_id: SessionId,
    pub path: std::path::PathBuf,
    pub status: SessionDirStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessUiDir {
    pub path: std::path::PathBuf,
}

/// The harness announces all available models as `provider/model` strings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessModelsAvailable {
    /// Each entry is `"provider_name/model_id"`.
    pub models: Vec<ModelId>,
}

/// The harness announces role names with resolved descriptions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoleInfo {
    /// Stable role name accepted by `ui.role_select`.
    pub name: String,
    /// Human-readable summary of the role's resolved model and knobs.
    pub description: String,
    /// Optional free-form role summary from harness configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_description: Option<String>,
}

/// The harness announces all roles available for selection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRolesAvailable {
    /// Role entries sorted by name for deterministic UI menus.
    pub roles: Vec<HarnessRoleInfo>,
}

/// The harness announces the selected role and its currently resolved model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoleSelected {
    /// Selected agent role. Role selection is always the runtime source of
    /// truth; the model is derived from this role and provider availability.
    pub role: String,
    /// Model currently resolved for [`Self::role`], or `None` while the role's
    /// model is not provider-published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Total context window size, in tokens, if known for the resolved model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Effective role/provider baseline parameters, ignoring persisted state.
    /// The UI compares live parameters against this baseline so state overrides
    /// stay visible in the status bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_params: Option<ModelParams>,
}

/// Current context usage for the selected model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessContextUsageChanged {
    /// Input tokens consumed by the most recent agent response, if the
    /// provider reported it. `None` means usage has never been
    /// reported for the current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent response,
    /// if the provider reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Percentage of the context window currently used. `None` when
    /// the selected provider model metadata is unavailable, so the UI
    /// can fall back to showing raw token count instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<u8>,
}

/// Reasoning effort level. Maps to provider-specific reasoning
/// controls (OpenAI `reasoning.effort`, Anthropic
/// `thinking.budget_tokens`). `Off` disables it entirely.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Effort {
    #[default]
    Off = 0,
    Minimal = 1,
    Low = 2,
    Medium = 3,
    High = 4,
    /// `rename_all = "snake_case"` would emit `x_high` for this
    /// variant, but the canonical wire string is `xhigh` everywhere
    /// else (`/role engineer effort xhigh`, OpenAI's `reasoning_effort` field,
    /// `Display`, `FromStr`, `effort_wire`). Pin it explicitly so
    /// serde-driven config paths (`default_efforts`,
    /// `reasoningEfforts`) agree with the rest.
    #[serde(rename = "xhigh")]
    XHigh = 5,
}

impl Effort {
    /// Cycles to the next level (wraps `XHigh → Off`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Off,
        }
    }

    /// Cycle in the canonical order, but only through levels that are
    /// in `allowed` so callers don't land on a level the current model
    /// doesn't support (e.g. xhigh on `gpt-5.4-mini`). Falls back to
    /// [`Effort::next`] when `allowed` is empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        // Bounded by `Effort` variant count — at most one full
        // wrap-around before we either land on an allowed level or
        // confirm none exist.
        for _ in 0..6 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display (`off` / `low` / `high` / etc).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    /// Numeric tag suitable for storing in an `AtomicU8`. Round-trips
    /// through [`Effort::from_u8`].
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Effort::as_u8`]. Returns `None` for unknown tags so
    /// callers can decide how to recover; the common case (loading from
    /// an atomic mirror) maps `None` to [`Effort::Off`].
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Minimal),
            2 => Some(Self::Low),
            3 => Some(Self::Medium),
            4 => Some(Self::High),
            5 => Some(Self::XHigh),
            _ => None,
        }
    }

    /// True for the default level (`Off`). Used by `ModelParams`'
    /// `#[serde(skip_serializing_if)]` so untouched values stay out
    /// of the wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Off)
    }
}

impl std::str::FromStr for Effort {
    type Err = ParseEffortError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            other => Err(ParseEffortError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when an effort string is not one of the well-known
/// levels (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseEffortError {
    input: String,
}

impl ParseEffortError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseEffortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown effort level `{}`; expected off/minimal/low/medium/high/xhigh",
            self.input
        )
    }
}

impl std::error::Error for ParseEffortError {}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces the current effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessEffortChanged {
    pub level: Effort,
}

/// Optional upstream service tier. `Fast` enables Fast mode on providers
/// that expose it; `Flex` is an explicit lower-priority service tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Flex,
}

impl ServiceTier {
    /// Config/event spelling used by Codex (`fast` / `flex`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Flex => "flex",
        }
    }

    /// OpenAI wire spelling used by Codex requests (`priority` / `flex`).
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Fast => "priority",
            Self::Flex => "flex",
        }
    }
}

/// The harness announces the current service tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessServiceTierChanged {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

/// Output verbosity hint sent to providers that support it (OpenAI
/// GPT-5 family: `verbosity` on Chat Completions, `text.verbosity` on
/// Responses). Providers that don't advertise `supportsVerbosity`
/// silently ignore the field.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Verbosity {
    #[default]
    Low = 0,
    Medium = 1,
    High = 2,
}

impl Verbosity {
    /// Cycles to the next level (wraps `High → Low`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Low,
        }
    }

    /// Cycle in canonical order through levels that are in `allowed`.
    /// Falls back to plain [`Verbosity::next`] when `allowed` is empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        for _ in 0..3 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display (`low` / `medium` / `high`).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Low),
            1 => Some(Self::Medium),
            2 => Some(Self::High),
            _ => None,
        }
    }

    /// Wire string for OpenAI's `verbosity` / `text.verbosity` field.
    /// All variants map to a non-empty string — there is no "off"
    /// sentinel — so callers gate the field on a provider-level
    /// `supports_verbosity` flag, not on the value itself.
    #[must_use]
    pub const fn as_openai_wire(self) -> &'static str {
        self.as_str()
    }

    /// True for the default level. Used by `#[serde(skip_serializing_if)]`
    /// on `ModelParams` so untouched values stay out of the wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Low)
    }
}

impl std::str::FromStr for Verbosity {
    type Err = ParseVerbosityError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(ParseVerbosityError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when a verbosity string is not one of the well-known
/// levels (`low`, `medium`, `high`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseVerbosityError {
    input: String,
}

impl ParseVerbosityError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseVerbosityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown verbosity level `{}`; expected low/medium/high",
            self.input
        )
    }
}

impl std::error::Error for ParseVerbosityError {}

impl fmt::Display for Verbosity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces the current verbosity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessVerbosityChanged {
    pub level: Verbosity,
}

/// The harness announces which verbosity levels are valid for the
/// selected role's resolved model. Updated on startup and whenever the
/// resolved model changes. Empty list means the selected role has no
/// resolved model; a single-element `[Medium]` list means the provider
/// doesn't support the knob.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessVerbositiesAvailable {
    pub levels: Vec<Verbosity>,
}

/// Whether to ask the provider for a human-readable summary of its
/// reasoning, and at what verbosity. Currently only the OpenAI
/// Responses API exposes this surface (`reasoning.summary`). Auto by
/// default for providers that advertise `supportsReasoningSummary`;
/// `Off` everywhere else.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ThinkingSummary {
    #[default]
    Off = 0,
    Auto = 1,
    Concise = 2,
    Detailed = 3,
}

impl ThinkingSummary {
    /// Cycles to the next level (wraps `Detailed → Off`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Auto,
            Self::Auto => Self::Concise,
            Self::Concise => Self::Detailed,
            Self::Detailed => Self::Off,
        }
    }

    /// Cycle in canonical order through levels that are in `allowed`.
    /// Falls back to plain [`ThinkingSummary::next`] when `allowed` is
    /// empty.
    #[must_use]
    pub fn next_in(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self.next();
        }
        let mut candidate = self.next();
        for _ in 0..4 {
            if allowed.contains(&candidate) {
                return candidate;
            }
            candidate = candidate.next();
        }
        self
    }

    /// Short label for status display.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Concise => "concise",
            Self::Detailed => "detailed",
        }
    }

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::Auto),
            2 => Some(Self::Concise),
            3 => Some(Self::Detailed),
            _ => None,
        }
    }

    /// Wire string used by OpenAI's Responses `reasoning.summary`
    /// field, or `None` for the off mode where the field is omitted.
    #[must_use]
    pub const fn as_openai_wire(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Auto => Some("auto"),
            Self::Concise => Some("concise"),
            Self::Detailed => Some("detailed"),
        }
    }

    /// True for the default level.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Off)
    }
}

impl std::str::FromStr for ThinkingSummary {
    type Err = ParseThinkingSummaryError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            "concise" => Ok(Self::Concise),
            "detailed" => Ok(Self::Detailed),
            other => Err(ParseThinkingSummaryError {
                input: other.to_owned(),
            }),
        }
    }
}

/// Error returned when a thinking-summary string is not one of the
/// well-known modes (`off`, `auto`, `concise`, `detailed`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseThinkingSummaryError {
    input: String,
}

impl ParseThinkingSummaryError {
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for ParseThinkingSummaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown thinking summary `{}`; expected off/auto/concise/detailed",
            self.input
        )
    }
}

impl std::error::Error for ParseThinkingSummaryError {}

impl std::fmt::Display for ThinkingSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The harness announces the current thinking-summary mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessThinkingSummaryChanged {
    pub level: ThinkingSummary,
}

/// The harness announces which thinking-summary modes are valid for
/// the selected role's resolved model. Empty list means the selected role has
/// no resolved model; `[Off]` means the provider doesn't expose summaries.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessThinkingSummariesAvailable {
    pub levels: Vec<ThinkingSummary>,
}

/// Per-prompt model knobs the harness selects, persists, and stamps
/// onto every [`SessionPromptCreated`]. Bundling these together lets
/// providers and backends thread one struct through instead of a
/// growing list of fields. Each component independently falls back to
/// "omit the field" when its [`Verbosity::is_default`] / `is_default`
/// helper says it's still at the default.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelParams {
    #[serde(default, skip_serializing_if = "Effort::is_default")]
    pub effort: Effort,
    #[serde(default, skip_serializing_if = "Verbosity::is_default")]
    pub verbosity: Verbosity,
    #[serde(default, skip_serializing_if = "ThinkingSummary::is_default")]
    pub thinking_summary: ThinkingSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
}

/// The harness announces which efforts are valid for the selected role's
/// resolved model. Updated on startup and whenever the resolved model changes.
/// Empty list means no effort applies (the selected role has no resolved model,
/// or the provider doesn't support reasoning).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessEffortsAvailable {
    pub levels: Vec<Effort>,
}

// ---------------------------------------------------------------------------
// Tool events
// ---------------------------------------------------------------------------

/// Tool metadata used during registration and invocation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    #[default]
    Function,
    Custom,
}

impl ToolType {
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Function)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGrammarSyntax {
    Lark,
    Regex,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolFormat {
    Text,
    Grammar {
        syntax: ToolGrammarSyntax,
        definition: String,
    },
}

/// Tool metadata used during registration and invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_visible_name: Option<ToolName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this is a JSON-schema function tool or a freeform custom tool.
    pub tool_type: ToolType,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Optional freeform/custom input format. `None` means provider-default
    /// unconstrained text for custom tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ToolFormat>,
    /// Whether this tool should be advertised to the agent when the role has
    /// no explicit `tools` allow-list and `disableTools` does not remove it.
    #[serde(default = "tool_enabled_by_default", skip_serializing_if = "is_true")]
    pub enabled_by_default: bool,
    /// Execution mode used by the harness dispatch state machine.
    /// Shared calls may overlap with each other; exclusive calls run
    /// alone within their conversation. Unknown / unset declarations
    /// default to [`ToolExecutionMode::Exclusive`] so extensions that
    /// haven't been updated don't silently lose ordering.
    #[serde(default, alias = "side_effects")]
    pub execution_mode: ToolExecutionMode,
    /// Whether the harness may close the model-visible foreground turn before
    /// the real tool process has returned. `None` means the harness applies its
    /// default policy, currently
    /// [`BackgroundSupport::MinForegroundSeconds`]`(5)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_support: Option<BackgroundSupport>,
}

const fn tool_enabled_by_default() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

/// How the harness may schedule a tool call relative to other tool calls.
///
/// This is informational for the agent and enforced by the harness's tool
/// dispatch queue.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    /// May run concurrently with other shared calls in the same conversation.
    #[serde(alias = "pure")]
    Shared,
    /// Must run alone within its conversation. Independent conversations can
    /// run their own exclusive calls in parallel. Default so tools that do not
    /// explicitly opt in to shared scheduling are treated conservatively.
    #[default]
    #[serde(alias = "mutating")]
    Exclusive,
}

/// Backward-compatible name for [`ToolExecutionMode`].
#[deprecated(note = "use ToolExecutionMode")]
pub type ToolSideEffects = ToolExecutionMode;

/// Foreground/background policy for a tool call after dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundSupport {
    /// Close the foreground as soon as the tool is dispatched.
    Instant,
    /// Keep the call in the foreground for at least this many seconds.
    MinForegroundSeconds(u64),
    /// Never synthesize foreground completion before the real result arrives.
    Never,
}

impl BackgroundSupport {
    /// Effective background support when a tool registration omits the field.
    #[must_use]
    pub const fn default_effective() -> Self {
        Self::MinForegroundSeconds(5)
    }
}

/// Per-prompt knob telling the provider whether the model is allowed
/// to call tools on this turn. Stamped onto every
/// [`SessionPromptCreated`]; the harness sets [`Self::None`] for
/// non-tool extension-side queries (e.g. `std-notifications`' idle
/// summary) so the cache prefix (tools + system_prompt) stays
/// byte-identical to the parent conv's while still preventing the
/// summarizer from accidentally calling `write` / `edit` / `delegate`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call tools (provider default).
    #[default]
    Auto,
    /// The model must produce a text answer this turn; tools are
    /// still declared in the request (so cache prefix matches), but
    /// the provider rejects tool-call output.
    None,
}

impl ToolChoice {
    /// True for the default value. Used by `#[serde(skip_serializing_if)]`
    /// on [`SessionPromptCreated`] so untouched values stay out of the
    /// wire form.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRegister {
    /// Tool metadata made available to the agent and used for routing calls.
    pub tool: ToolSpec,
    /// Optional system-prompt fragment template to render whenever this tool is
    /// enabled for the current role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_fragment: Option<PromptFragment>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolUnregister {
    pub tool_name: ToolName,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub arguments: CborValue,
    /// Who started the prompt that produced this tool call. The
    /// harness stamps this from the call's owning conversation so
    /// subscribers can tell main-agent tool activity from sub-agent
    /// (delegate / extension-query) tool activity without having to
    /// map `call_id` back to a conversation themselves.
    #[serde(default)]
    pub originator: PromptOriginator,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInvoke {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: CborValue,
    /// Echo of [`ToolRequest::originator`]. Tools usually don't
    /// branch on it, but it's available for logging / progress
    /// tagging / policy decisions that depend on whether the call
    /// is for the main agent or a sub-agent.
    #[serde(default)]
    pub originator: PromptOriginator,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub result: CborValue,
    /// Optional UI display descriptor populated by the tool. When
    /// present, lets the renderer paint a uniform tool block without
    /// inspecting `result`'s tool-specific shape. This is operational
    /// display metadata, not transcript truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
    /// Echo of the originating [`ToolRequest::originator`]. Tool
    /// extensions usually pass [`PromptOriginator::User`] (the
    /// default); the harness re-stamps this with the call's owning
    /// conversation's originator before broadcasting, so subscribers
    /// see a faithful tag without every extension having to track
    /// it.
    #[serde(default)]
    pub originator: PromptOriginator,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
    /// See [`ToolResult::display`]. On error, the descriptor's
    /// `status` is typically [`ToolDisplayStatus::Error`] and
    /// `status_text` carries an optional error label. Renderers add the
    /// generic error prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
    /// Echo of the originating [`ToolRequest::originator`]; see
    /// [`ToolResult::originator`].
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Real success result for a tool call whose foreground was already completed
/// with a synthetic background placeholder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolBackgroundResult {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub result: CborValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// Real error result for a tool call whose foreground was already completed
/// with a synthetic background placeholder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolBackgroundError {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CborValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// UI display descriptor for one finished tool call.
///
/// Populated by the tool side (in-tree dispatchers or out-of-tree
/// extensions) and rendered uniformly by the CLI without inspecting
/// the tool's specific result shape. Carries everything the chip line
/// needs (args label, info chips, stats, status word) plus an
/// optional rich payload to render in a block below.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolDisplay {
    /// Short label rendered alongside the tool name (e.g.
    /// `"src/main.rs"`, `"\"foo\" in src"`, `"git status"`). Empty
    /// when the tool has nothing useful to surface beyond its name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// Compact `NM, NL, NkB`-style stats. Each field is optional
    /// so the renderer can omit a slot rather than emit `(0M, 1L)`.
    #[serde(default, skip_serializing_if = "ToolDisplayStats::is_empty")]
    pub stats: ToolDisplayStats,
    /// Labelled counter chips (current / optional total) rendered
    /// between stats and `info_chips`. Used for tools that surface
    /// progress data: `#12.3k/200k`, `%3`, `bytes: 12/200`,
    /// etc. The unit hint picks the rendering shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress_counters: Vec<ProgressCounter>,
    /// Free-form info chips beyond the stats slot (e.g. `"(2
    /// suggestions)"`, `"(3 entries)"`). Rendered between counters
    /// and status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub info_chips: Vec<String>,
    /// Severity of the trailing status chip. Picks its themed color.
    pub status: ToolDisplayStatus,
    /// Status word/message rendered as the last chip (e.g. `"ok"`,
    /// `"regex parse error"`). For
    /// [`ToolDisplayStatus::Error`], this is the label without the
    /// generic `"err:"` prefix; renderers add that prefix and handle any
    /// width abbreviation needed for the current UI.
    pub status_text: String,
    /// Optional rich content rendered in a block below the chip row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<ToolDisplayPayload>,
}

/// One labelled counter rendered as an info chip. Shape depends on
/// `unit` and which of `complete` / `total` are populated:
/// - `Count`: `N` (complete only) or `N/M` (both).
/// - `Percent`: `N%` (complete only) or `N%/M` (both — `M` is e.g. a context
///   window size, formatted like a token count).
/// - `Tokens`: `N` or `N/M` rendered with token-count suffixes.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProgressCounter {
    /// Human-readable prefix shown before the value (e.g. `"ctx"`,
    /// `"tools"`). Renders as `"label: value"`. `None` for an
    /// unlabelled chip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// What `complete` and `total` represent. Picks the rendering.
    pub unit: ProgressUnit,
    /// Completed amount. `None` is rendered as `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete: Option<u64>,
    /// Optional denominator. For `Count`, the cumulative total; for
    /// `Percent`, the underlying span (e.g. context window size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressUnit {
    /// Raw integers. Renders as `N` or `N/M`. Default if the sender
    /// doesn't specify.
    #[default]
    Count,
    /// `complete` is a percent 0..=100. Renders as `N%` or
    /// `N%/format_token_count(total)`.
    Percent,
    /// `complete` and `total` are token counts, each formatted with
    /// token-count suffixes.
    Tokens,
}

/// Volume metrics. Each is optional because a given tool typically
/// reports only some of them — `read` has lines/bytes but no matches;
/// `grep` has all three; `ls` has none (uses [`ToolDisplay::info_chips`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ToolDisplayStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl ToolDisplayStats {
    pub fn is_empty(&self) -> bool {
        self.matches.is_none() && self.lines.is_none() && self.bytes.is_none()
    }

    /// Build line and byte statistics for non-empty text.
    #[must_use]
    pub fn for_text(text: &str) -> Self {
        if text.is_empty() {
            return Self::default();
        }
        Self {
            matches: None,
            lines: Some(text.lines().count() as u64),
            bytes: Some(text.len() as u64),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDisplayStatus {
    #[default]
    Success,
    Warning,
    Error,
    /// The tool is still running. Used by progress events. The
    /// renderer trades the trailing status chip for
    /// [`crate::PROGRESS_INDICATOR_TEXT`].
    InProgress,
}

/// Rich content rendered below the chip row. Closed for now — extend
/// as new tool kinds need it. Tools that don't produce a rich payload
/// (most of them) leave [`ToolDisplay::payload`] as `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDisplayPayload {
    /// Structured file diff. The renderer derives the `+N -M` chip
    /// from the summary's `added`/`removed` and renders the hunks
    /// below the chip row.
    Diff(DiffSummary),
    /// Plain text rendered below the chip row. Used when the inline
    /// args label would be too noisy (e.g. multi-line shell commands).
    Text { text: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProgressUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolProgress {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressUpdate>,
}

/// Live snapshot of a sub-agent spawned by the `delegate` tool.
///
/// Emitted by the harness whenever the side conversation backing a
/// `delegate` invocation makes observable progress: a tool call starts
/// or finishes, or the sub-agent reports new context-token usage. The
/// CLI re-renders the running `delegate` tool block to surface this
/// to the user without persisting per-update history. Transient — not
/// folded into the durable session log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DelegateProgress {
    /// The original parent `delegate` call — the tool block under
    /// which this update should appear.
    pub call_id: ToolCallId,
    /// Display name the parent agent provided for the sub-task.
    pub task_name: String,
    /// Role used by the delegated sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Most recent percent-of-context-window the sub-agent reported,
    /// when its model's window size is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_percent: Option<u8>,
    /// Most recent input-token count the sub-agent reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_input_tokens: Option<u64>,
    /// Sub-agent's model context window size, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_window: Option<u64>,
    /// Number of tool calls currently in flight in the sub-agent.
    pub tools_in_flight: u32,
    /// Cumulative number of tool calls the sub-agent has started
    /// during this delegation (including completed and in-flight).
    pub tools_total: u32,
    /// UI display descriptor for the running delegate block. The
    /// harness fills this in from the fields above so the renderer
    /// can paint the progress generically without inspecting them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolDisplay>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancel {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCancelled {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub tool_type: ToolType,
}

// ---------------------------------------------------------------------------
// Extension supervision events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionStarting {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionReady {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionExited {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionRestarting {
    pub instance_id: crate::ExtensionInstanceId,
    pub extension_name: ExtensionName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An extension discovered a skill and is advertising it to the harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtSkillAvailable {
    pub name: SkillName,
    pub description: String,
    /// Absolute path to the skill file so the harness can read it.
    pub file_path: std::path::PathBuf,
    /// When true the harness should include this skill in the system prompt.
    pub add_to_prompt: bool,
}

/// An extension discovered one AGENTS.md file and is advertising it to the
/// harness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentsMdAvailable {
    /// Absolute path to the AGENTS.md file.
    pub file_path: std::path::PathBuf,
    /// Full file contents, sent eagerly so the harness can inject them
    /// without an extra tool round trip.
    pub content: String,
}

/// An extension finished broadcasting refreshed prompt context for one session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtensionContextReady {
    pub session_id: SessionId,
}

/// Arbitrary JSON value published by an extension for one session context key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionContextValue(pub serde_json::Value);

/// An extension publishes its complete session-scoped contribution for one key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtSessionContextPublish {
    /// Session this context belongs to.
    pub session_id: SessionId,
    /// Top-level context key exposed to templates under
    /// `session_context.<key>`.
    pub key: SessionContextKey,
    /// Complete JSON contribution from this extension for the key.
    pub value: SessionContextValue,
}

/// An extension publishes or replaces one extension-level prompt fragment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtPromptFragmentPublish {
    /// Fragment template to make available during prompt rendering.
    ///
    /// The harness keys replacement by `(source_connection_id, fragment.name)`;
    /// the same extension publishing the same name again replaces its previous
    /// fragment.
    pub fragment: PromptFragment,
}

/// An extension's request for the harness to dispatch a side prompt
/// to the agent.
///
/// The harness spawns a fresh conversation off the user's current
/// branch, treats the side prompt like any other turn (LLM call,
/// optional tool calls, final response), then routes the agent's
/// final text back to the requesting extension as
/// [`ExtAgentQueryResult`] with the same `query_id`.
///
/// Side conversations are persisted as real branches in the session
/// tree but tagged via [`PromptOriginator::Extension`] so UIs can
/// filter them out.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentQuery {
    /// Extension-assigned correlation id, echoed back on the result.
    pub query_id: String,
    /// User-style instruction text. Appended to the current
    /// conversation's history as a `User` message before dispatch.
    pub instruction: String,
    /// Requested agent role for this side conversation. Tool-backed
    /// delegate queries default to `engineer`; non-tool queries without a role
    /// keep using the currently selected interactive role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Global sub-agent scheduling mode enforced by the harness.
    ///
    /// Shared queries may overlap with other shared sub-agent queries.
    /// Exclusive queries wait until no incompatible sub-agent query is active,
    /// then block later independent sub-agent queries until they finish.
    /// Defaults to Shared for compatibility with older extensions that did
    /// not declare global scheduling needs.
    #[serde(default = "default_ext_agent_query_execution_mode")]
    pub execution_mode: ToolExecutionMode,
    /// Input stats for the extension-provided instruction, excluding
    /// any private prefix the extension may have added.
    #[serde(default, skip_serializing_if = "ToolDisplayStats::is_empty")]
    pub input_stats: ToolDisplayStats,
    /// `ToolCallId` of the tool invocation that triggered this query,
    /// when the extension is implementing a tool whose live progress
    /// the harness should attribute back to that call. Used by the
    /// `delegate` tool: the harness emits [`DelegateProgress`] under
    /// this id as the side conversation runs. Optional — non-tool
    /// extensions issuing queries leave it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// Human-readable name for the delegated task, surfaced in the
    /// UI alongside [`DelegateProgress`]. Optional for the same reason
    /// `tool_call_id` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_name: Option<String>,
}

const fn default_ext_agent_query_execution_mode() -> ToolExecutionMode {
    ToolExecutionMode::Shared
}

/// Reply to an [`ExtAgentQuery`], routed point-to-point back to the
/// extension that issued it. `text` is the agent's final answer
/// (empty when `error` is set).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExtAgentQueryResult {
    pub query_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Metadata for one model currently served by a provider extension.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelInfo {
    /// Fully-qualified model id. The provider segment is part of user-visible
    /// selection and harness routing.
    pub id: ModelId,
    /// Optional human-friendly label. UIs may fall back to [`Self::id`] when it
    /// is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Provider-published preference for becoming the implicit default model
    /// when the selected role does not name one. Higher values win; ties are
    /// broken by model id for deterministic behavior. Zero means neutral.
    #[serde(default, skip_serializing_if = "is_default_affinity_neutral")]
    pub default_affinity: i32,
    /// Total model context window in tokens. Required so harness/UI state does
    /// not have to fall back to provider-specific config.
    pub context_window: u64,
    /// Reasoning-effort levels accepted by this model, in UI cycling order.
    pub efforts: Vec<Effort>,
    /// Output-verbosity levels accepted by this model, in UI cycling order.
    pub verbosities: Vec<Verbosity>,
    /// Thinking-summary modes accepted by this model, in UI cycling order.
    pub thinking_summaries: Vec<ThinkingSummary>,
    /// Whether this model can use the provider's standalone compaction
    /// endpoint.
    #[serde(default)]
    pub supports_compaction: bool,
}

/// Provider extension snapshot of its currently available models.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelsUpdated {
    /// Complete replacement snapshot for the sending extension. Publishing an
    /// empty list means the extension currently serves no models.
    pub models: Vec<ProviderModelInfo>,
}

/// Extension-defined event payload.
///
/// `name` is the dotted event name used for routing and subscription
/// matching. `payload` carries extension-owned CBOR data. When
/// `session_id` is set, the harness may include the event in that
/// session's durable event log according to the surrounding emit
/// metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomEvent {
    pub name: EventName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub payload: CborValue,
}

// ---------------------------------------------------------------------------
// UI events — facts from the user interface
// ---------------------------------------------------------------------------

/// The user submitted a prompt in the UI.
///
/// `originator` is normally [`PromptOriginator::User`] — the field
/// exists so the harness can re-use this event type when dispatching
/// side queries spawned by extensions: the appended user-style
/// instruction also flows as a `UiPromptSubmitted` (so it folds into
/// the session tree), but UIs and other extensions filter on
/// `originator.is_user()` to avoid rendering side conversations as
/// real user turns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptSubmitted {
    pub session_id: SessionId,
    pub text: String,
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Free-form correlation tag chosen by the submitter and copied
    /// forward onto the first [`SessionPromptCreated`] the harness
    /// emits for this prompt. Lets a client (notably the test helper
    /// in `tau-harness::daemon`) match the response chain to the
    /// submission it made, without relying on event ordering or
    /// re-using a long-lived connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
}

/// A trailing-edge debounced snapshot of the in-progress prompt the
/// user is composing in the UI. Emitted at most once per second
/// while the user is typing; carries the full current contents of
/// the prompt buffer.
///
/// Always transient — never persisted to the per-session event log,
/// never folded into the session tree. Subscribers use it to detect
/// "user is alive" without polling: e.g. std-notifications resets
/// its idle deadline on every draft event so the desktop notification
/// doesn't fire while the user is mid-sentence.
///
/// Future consumers might use the text for autocomplete, draft
/// restoration on UI reconnect, or in-progress prompt sync across
/// multiple attached UIs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiPromptDraft {
    pub session_id: SessionId,
    pub text: String,
}

/// The UI is detaching and wants the daemon to stay alive after it
/// leaves, so a later `tau --attach` can pick up the same
/// session. The harness flips its `exit_on_disconnect` flag to
/// `false` on receipt.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiDetachRequest {}

/// The user requests switching to an agent role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRoleSelect {
    /// Role name to make the runtime source of truth for model resolution.
    pub role: String,
}

/// The user changes or deletes an agent role.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRoleUpdate {
    /// Role name whose runtime override should change.
    pub role: String,
    /// Typed mutation to apply to the role override.
    pub action: UiRoleUpdateAction,
}

/// Typed role mutation requested by a UI. `None` fields clear the explicit
/// role value so normal model-specific fallback resolution applies.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiRoleUpdateAction {
    /// Remove this role's runtime override, or delete the runtime-only role.
    Delete,
    /// Set or clear the role's preferred model.
    SetModel {
        /// Model to pin this role to, or `None` to use the first available
        /// model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelId>,
    },
    /// Set or clear the role's reasoning effort.
    SetEffort {
        /// Reasoning effort to store, or `None` to use the model fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
    },
    /// Set or clear the role's output verbosity.
    SetVerbosity {
        /// Output verbosity to store, or `None` to use the model fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verbosity: Option<Verbosity>,
    },
    /// Set or clear the role's thinking-summary mode.
    SetThinkingSummary {
        /// Thinking-summary mode to store, or `None` to use the model fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_summary: Option<ThinkingSummary>,
    },
    /// Set or clear the role's provider service tier.
    SetServiceTier {
        /// Service tier to store, or `None` to use the provider default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service_tier: Option<ServiceTier>,
    },
    /// Set or clear the role's explicit tool allow-list.
    SetTools {
        /// Internal tool names to allow, or `None` to use default tool
        /// enablement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<ToolName>>,
    },
    /// Set the role's explicit tool block-list.
    SetDisableTools {
        /// Internal tool names to disable even when enabled by default or
        /// explicitly allowed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        disable_tools: Vec<ToolName>,
    },
}

/// The user requests switching to a different session within the same
/// daemon. Harness emits `SessionShutdown` for the current session,
/// then `SessionStarted { reason: New | Resume }` for the new one,
/// and waits for extensions to acknowledge re-init.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiSwitchSession {
    pub new_session_id: SessionId,
    /// `New` if the id was just minted, `Resume` if it points at an
    /// existing session on disk.
    pub reason: SessionStartReason,
}

/// The user typed `/tree`: render the session's branching tree (one
/// `harness.info` line per node) to the chat output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiTreeRequest {
    pub session_id: SessionId,
}

/// The user typed `/tree <id>`: move the session's head pointer to the
/// given node, so the next prompt branches off there.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiNavigateTree {
    pub session_id: SessionId,
    pub node_id: u64,
}

/// The user typed `/compact`: force a provider-side compaction pass on
/// the current session history before the next prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiCompactRequest {
    pub session_id: SessionId,
}

/// Stop advancing an in-flight prompt at the next harness boundary.
///
/// Originally tied to the user typing `/cancel`, now also published
/// by the harness itself to preempt non-tool extension side
/// conversations when a user prompt arrives. The optional
/// [`Self::session_prompt_id`] disambiguates the two cases:
///
/// - `None` — broadcast cancel (the legacy `/cancel` semantics). The harness
///   clears the default conversation; the agent aborts whatever prompt it's
///   currently retry-sleeping on.
/// - `Some(spid)` — targeted cancel. The agent only aborts if the in-flight
///   prompt's spid matches; otherwise the frame is left in the retry-loop's
///   deferred buffer so the wrong prompt isn't collateral damage. The agent
///   serializes prompt processing internally, so a cancel published while the
///   spid in question is still queued (not yet dequeued from the agent's frame
///   channel) is harmless — it just falls through with no in-flight match.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiCancelPrompt {
    /// Session whose active or queued prompt should be cancelled.
    pub session_id: SessionId,
    /// Optional target. See struct doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_prompt_id: Option<SessionPromptId>,
}

/// Request that the harness remove and return the most recently queued user
/// prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiRecallQueuedPrompt {
    /// Session whose default conversation queue should be recalled from.
    pub session_id: SessionId,
}

/// Which stream a [`ShellCommandProgress`] chunk came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellStream {
    Stdout,
    Stderr,
}

/// The user submitted a `!`/`!!` shell command.
///
/// `include_in_context`: when `true` (from `!<cmd>`), the harness
/// injects a tagged user message containing the command and its
/// output into the session's conversation history on completion, so
/// the agent sees it on its next turn. When `false` (from `!!<cmd>`),
/// the result is UI-only and never reaches the model.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UiShellCommand {
    pub session_id: SessionId,
    pub command_id: crate::ShellCommandId,
    pub command: String,
    pub include_in_context: bool,
}

/// A chunk of output from a running user-initiated shell command.
/// Correlated to the request by `command_id`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandProgress {
    pub command_id: crate::ShellCommandId,
    pub stream: ShellStream,
    pub chunk: String,
}

/// A user-initiated shell command completed (exited or was cancelled).
///
/// The extension echoes `command`, `session_id`, and
/// `include_in_context` back from the originating `UiShellCommand`
/// so the harness can act on the finished event without bookkeeping
/// a per-command_id map.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellCommandFinished {
    pub command_id: crate::ShellCommandId,
    pub session_id: SessionId,
    pub command: String,
    pub include_in_context: bool,
    /// Interleaved stdout + stderr (truncated), the same shape the
    /// `shell` tool returns.
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------
// Term events — terminal-output side effects directed at the UI
// ---------------------------------------------------------------------------

/// Ask the UI to write an iTerm2 OSC 1337 `SetUserVar` escape sequence
/// to its terminal. The terminal emulator interprets it as setting
/// the named user variable (visible from terminal multiplexers and
/// scripts watching status); the visible terminal output does not
/// change. Useful for surfacing notifications, build status, or any
/// other state to terminal-side tooling.
///
/// The UI base64-encodes `value` and emits the appropriate escape
/// sequence form (plain, or `\x1bPtmux;...\x1b\\` wrapped when running
/// inside `tmux`). Components without access to a terminal — or
/// running through a UI that ignores the event — are no-ops.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Osc1337SetUserVar {
    /// User-variable name. Must be printable ASCII without `=` or
    /// control characters; the UI does not validate this and passes
    /// it through verbatim.
    pub name: String,
    /// Value to associate with `name`. Arbitrary bytes are fine — the
    /// UI base64-encodes before transmission.
    pub value: String,
}

// ---------------------------------------------------------------------------
// Session events — facts from the harness session tracker
// ---------------------------------------------------------------------------

/// The harness queued a user prompt because the agent is busy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptQueued {
    /// Session that owns the queued prompt.
    pub session_id: SessionId,
    /// Queued prompt text.
    pub text: String,
}

/// The harness recalled a previously queued user prompt for editing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptRecalled {
    /// Session that owns the recalled prompt.
    pub session_id: SessionId,
    /// Recalled prompt text.
    pub text: String,
}

/// A previously queued user prompt that the harness folded into the
/// in-flight turn as a steering message — appended to the next
/// `SessionPromptCreated` for this conversation alongside tool results,
/// rather than waiting for the conversation to return to `Idle` and
/// kicking off a fresh turn.
///
/// Folds into the `SessionTree` the same way as `UiPromptSubmitted`
/// and `SessionUserMessageInjected`: appending one `UserMessage` entry
/// at the current head. UIs typically react by promoting their
/// "(queued)" rendering of this prompt to a regular user message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptSteered {
    pub session_id: SessionId,
    pub text: String,
}

/// Why a `SessionStarted` was published. Lets extensions distinguish
/// "first session of this harness's life" from "user switched to a new
/// session" (e.g. so they can clear caches).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartReason {
    /// The harness eagerly initialized this session at startup.
    Initial,
    /// The user requested a fresh session via `/session new`.
    New,
    /// The user resumed an existing session by id.
    Resume,
}

/// The harness created or switched to a session. Extensions that
/// subscribe react by performing per-session setup (e.g. discovering
/// AGENTS.md) and signal completion with `ExtensionContextReady`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub session_id: SessionId,
    #[serde(default = "default_session_start_reason")]
    pub reason: SessionStartReason,
}

fn default_session_start_reason() -> SessionStartReason {
    SessionStartReason::Initial
}

/// The harness is leaving the current session. Fired before
/// `SessionStarted` for the next one when the user switches sessions.
/// Extensions that hold per-session state subscribe to flush or drop it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionShutdown {
    pub session_id: SessionId,
}

/// A synthetic user message injected into the session by the harness
/// (not authored by the human user directly). Sources include
/// `!`-prefixed shell command output and the eager AGENTS.md context
/// preamble. Carries the fully-rendered text so session replay does
/// not need to re-run any harness-side formatter; the SessionTree
/// folder treats this event the same as `UiPromptSubmitted` —
/// appending one `UserMessage` entry at the current head.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionUserMessageInjected {
    pub session_id: SessionId,
    pub text: String,
}

/// Who initiated the prompt — the human user via the UI, or an
/// extension via [`ExtAgentQuery`].
///
/// The provider's only obligation is to copy the originator from the
/// incoming [`SessionPromptCreated`] onto its outgoing
/// [`ProviderResponseFinished`]. The harness reads it on the way back
/// to decide whether the response is a normal turn (route to UI,
/// keep `default_conversation` advancing) or a side-query reply
/// (route an [`ExtAgentQueryResult`] to the requesting extension and
/// tear the conversation down).
///
/// UIs filter on `originator.is_user()` to ignore side conversations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptOriginator {
    /// Default — interactive prompt submitted through the UI.
    #[default]
    User,
    /// Side prompt requested by an extension via [`ExtAgentQuery`].
    Extension {
        name: ExtensionName,
        query_id: String,
    },
}

impl PromptOriginator {
    /// True iff this prompt is the user's interactive turn.
    #[must_use]
    pub const fn is_user(&self) -> bool {
        matches!(self, Self::User)
    }
}

/// Reference to tool definitions carried by an earlier prompt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptToolsRef {
    /// Prompt whose materialized tools contain the full tool list.
    pub base_session_prompt_id: SessionPromptId,
}

/// The harness persisted a normal assistant-generation prompt and
/// assigned it an ID.
///
/// Carries the assembled conversation context for the provider's normal
/// response path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptCreated {
    pub session_prompt_id: SessionPromptId,
    pub session_id: SessionId,
    /// System prompt sent alongside the item timeline.
    pub system_prompt: String,
    /// Fully materialized context items for this turn.
    pub context_items: Vec<ContextItem>,
    /// Tool definitions, or empty when [`Self::tools_ref`] is set.
    pub tools: Vec<ToolDefinition>,
    /// Optional reference to full tool definitions from an earlier prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_ref: Option<PromptToolsRef>,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Per-prompt model knobs (reasoning effort, output verbosity,
    /// thinking-summary mode). The harness stamps in its current
    /// selection on every prompt; backends pass each field through
    /// only when the provider advertises support for it.
    #[serde(default)]
    pub model_params: ModelParams,
    /// Whether tool calls are allowed on this turn. Defaults to
    /// `Auto`; the harness flips to `None` for non-tool extension
    /// side queries (e.g. idle-summary) so they cannot trigger
    /// destructive tools. Backends emit this as `tool_choice: "none"`
    /// on the upstream request body.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Who asked for this prompt. Defaults to [`PromptOriginator::User`]
    /// for backward compatibility with old persisted events.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// When `true`, the backend uses the **user's** `prompt_cache_key`
    /// bucket for this turn even though [`Self::originator`] is an
    /// extension. The harness sets this for non-fan-out side queries
    /// (notably `std-notifications`' idle-summary) so a single side
    /// turn can hit the user's already-warm prefix cache. Delegate
    /// sub-agents leave it `false` because parallel fan-out on a
    /// shared key would exceed OpenAI's 15 RPM-per-bucket guideline
    /// and degrade routing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub share_user_cache_key: bool,
    /// Echo of [`UiPromptSubmitted::ctx_id`] when this prompt was
    /// initiated by a UI submission. Tool-result follow-up
    /// `SessionPromptCreated` events for the same chain do not
    /// inherit it — only the first one does — so a correlator should
    /// capture the resulting [`Self::session_prompt_id`] and track
    /// the rest of the chain by spid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_id: Option<String>,
    /// Hint for backends that support stateful chaining: the most
    /// recent provider response id from this conversation and the item
    /// index where the new turn's content begins. Backends that do not
    /// support chaining ignore this and use the full materialized
    /// `context_items` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_candidate: Option<PreviousResponseCandidate>,
}

/// The harness assembled a provider-side compaction request and
/// assigned it an ID.
///
/// Compaction reuses the same prompt-delivery and materialization
/// scheme as [`SessionPromptCreated`], but it is a distinct provider
/// operation with its own event name so consumers do not need to infer
/// alternate semantics from a mode flag on a normal prompt event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCompactionRequested {
    #[serde(flatten)]
    pub prompt: SessionPromptCreated,
}

/// Best-effort provider-side prompt-cache prewarm request.
///
/// Carries the same stable prefix fields as the first real
/// [`SessionPromptCreated`] but intentionally has no
/// [`SessionPromptId`], no user task prompt, and no
/// `previous_response_id`. Providers that support a non-generating
/// upstream call may send it; all others no-op.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptPrewarmRequested {
    pub session_id: SessionId,
    pub system_prompt: String,
    pub context_items: Vec<ContextItem>,
    pub tools: Vec<ToolDefinition>,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Per-prompt model knobs, matching the first real prompt.
    #[serde(default)]
    pub model_params: ModelParams,
    /// Whether tool calls are allowed on the warmed prefix.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Prewarm only warms the interactive user's cache bucket.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Preserve the first real user prompt's cache-key derivation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub share_user_cache_key: bool,
}

/// A provider-side compaction pass has started for this session.
///
/// This is a transient lifecycle event for clients to render progress;
/// successful compaction is recorded durably by [`SessionCompacted`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionCompactionStarted {
    pub session_id: SessionId,
    /// Conversation that owns this compaction. UIs use this to hide
    /// sub-agent compaction lifecycle blocks from the main transcript.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Input-token count of the conversation before compaction, if known.
    /// Renderers use this as the `#…` context-size chip on the live status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_input_tokens: Option<u64>,
}

/// Final status of a provider-side compaction lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCompactionOutcome {
    Succeeded,
    Failed,
}

/// A provider-side compaction pass finished.
///
/// This is transient UI/status metadata. On success, a separate
/// [`SessionCompacted`] event carries the durable compacted input items.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionCompactionFinished {
    pub session_id: SessionId,
    /// Conversation that owns this compaction. UIs use this to hide
    /// sub-agent compaction lifecycle blocks from the main transcript.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Input-token count of the conversation before compaction, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_input_tokens: Option<u64>,
    /// Prompt/input-token count of the compacted replacement window, if known.
    ///
    /// Providers do not always report usage for standalone compaction. When
    /// exact usage is unavailable, the harness may fill this with its
    /// documented prompt-size estimate for the provider-owned items that
    /// will be replayed after compaction. It is UI context-size metadata,
    /// not a billing counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_input_tokens: Option<u64>,
    pub outcome: SessionCompactionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The harness replaced earlier branch history with a compact summary.
///
/// The durable event log remains append-only: compaction does not delete
/// prior events from disk. Instead, prompt assembly treats this event as a
/// history reset point and replays only the summary plus the entries that
/// follow it on the current branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCompacted {
    pub session_id: SessionId,
    /// Conversation that owns this durable compaction summary. UIs use
    /// this to hide sub-agent compaction results from the main transcript.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Input-token count of the conversation before compaction, if known.
    /// Stored on the durable event so replayed UIs can render the same status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_input_tokens: Option<u64>,
    /// Prompt/input-token count of the compacted replacement window, if known.
    ///
    /// Providers do not always report usage for standalone compaction. When
    /// exact usage is unavailable, the harness may fill this with its
    /// documented prompt-size estimate for the provider-owned items that
    /// will be replayed after compaction. Stored on the durable event so
    /// replayed UIs can render the same status. It is UI context-size
    /// metadata, not a billing counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_input_tokens: Option<u64>,
    pub replacement_window: Vec<ContextItem>,
}

/// Reference to a prior turn's response, used to enable stateful
/// chaining on backends that support it. See
/// [`SessionPromptCreated::previous_response_candidate`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreviousResponseCandidate {
    /// `response.id` returned by the provider on the most recent
    /// successful turn for this conversation.
    pub provider_response_id: String,
    /// Index in [`SessionPromptCreated::context_items`] where items
    /// added since the prior response begin. Backends slicing for a
    /// delta call use `context_items[next_item_index..]`.
    pub next_item_index: usize,
    /// Backend that produced `provider_response_id`, when known.
    pub backend: ProviderBackend,
}

// ---------------------------------------------------------------------------
// Provider execution events — facts from the provider backend
// ---------------------------------------------------------------------------

/// The provider accepted a prompt and began processing it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderPromptSubmitted {
    /// Prompt id the provider accepted.
    pub session_prompt_id: SessionPromptId,
    /// Echo of [`SessionPromptCreated::originator`]. UIs and other
    /// extensions filter on `originator.is_user()` so provider work for a side
    /// conversation doesn't trigger user-facing
    /// effects like clearing an idle deadline.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// The provider has new accumulated response text for a prompt.
/// Each update carries the full text so far (replace, not delta).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseUpdated {
    /// Prompt id whose accumulated response changed.
    pub session_prompt_id: SessionPromptId,
    /// Full response text accumulated so far.
    pub text: String,
    /// Accumulated provider-supplied reasoning summary so far, if the
    /// provider exposed one. Replace, not delta. Persisted with the
    /// final assistant turn but never replayed back into later
    /// prompts (see `assemble_conversation`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Echo of [`SessionPromptCreated::originator`]. UIs filter on
    /// `originator.is_user()` so the streaming text from a side
    /// conversation doesn't paint into the user's chat window.
    #[serde(default)]
    pub originator: PromptOriginator,
}

/// The provider finished processing a prompt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStopReason {
    /// The model completed the turn without requesting any tool work.
    #[default]
    EndTurn,
    /// The model stopped because it emitted tool calls that Tau should run.
    ToolCalls,
    /// The response represents a compaction window rather than a normal turn.
    Compaction,
    /// The turn ended with an error message synthesized by Tau.
    Error,
}

impl ProviderStopReason {
    #[must_use]
    pub const fn requests_tool_calls(self) -> bool {
        matches!(self, Self::ToolCalls)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponseFinished {
    /// Prompt id the provider finished.
    pub session_prompt_id: SessionPromptId,
    /// Final provider output, including assistant messages, reasoning,
    /// compaction payloads, and/or requested tool calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_items: Vec<ContextItem>,
    /// Why the provider stopped this turn.
    pub stop_reason: ProviderStopReason,
    /// Echo of [`SessionPromptCreated::originator`]. The provider must
    /// copy this from the prompt; the harness routes the response
    /// based on it.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Provider-reported usage for this response, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderTokenUsage>,
    /// Which LLM backend handled this turn. Recorded once per turn
    /// (instead of in a trace line) so offline inspection of the
    /// event log can correlate cache-miss / retry patterns with the
    /// backend that produced them — e.g. distinguishing OpenAI
    /// public-API behavior from the ChatGPT Codex Responses backend.
    /// `None` for turns that never reached a backend (e.g. an
    /// provider-side resolution failure or the in-process echo provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<ProviderBackend>,
    /// Provider-supplied response id for this turn, when the backend
    /// exposed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    /// Per-turn delta of the provider's Codex WS pool counters. `Some(_)`
    /// only for Responses-backend turns where the WS path was
    /// attempted (i.e. `cfg.supports_websocket` and the per-session
    /// sticky-disable flag was off). `None` for Chat Completions and
    /// for Responses sessions that have been permanently flipped to
    /// HTTP+SSE. Lets offline analysis attribute a low
    /// `cached_tokens` to a chain-strip event (the Codex chain cache
    /// is connection-local; a fresh socket or a silent reconnect
    /// drops the in-request `previous_response_id`, collapsing
    /// `cached_tokens` to the static system+tools baseline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_pool_delta: Option<WsPoolDelta>,
}

/// Per-turn delta of the provider's Codex WebSocket pool counters. All
/// three counters are monotonic-since-process-start in the provider;
/// the harness records the *delta* incurred by a single turn so
/// offline analysis can attribute cache misses to WS-layer events.
///
/// A non-zero `silent_reconnects` or `chain_strips_on_fresh` on a
/// turn is the definitive signature of why that turn's
/// `previous_response_id` was stripped on the wire — and therefore
/// why its `cached_tokens` dropped to the static system+tools
/// baseline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsPoolDelta {
    /// Fresh sockets opened this turn. Counts every reason: cold
    /// pool, server-age purge, bearer rotation, silent-reconnect
    /// recovery.
    pub upgrades: u32,
    /// Cached sockets that died mid-turn and triggered the silent
    /// reopen-and-replay-without-chain-id recovery this turn.
    pub silent_reconnects: u32,
    /// Times the fresh-socket path stripped `previous_response_id`
    /// from the outgoing request this turn because the new socket's
    /// chain cache was empty by definition.
    pub chain_strips_on_fresh: u32,
}

/// Diagnostic emitted when a chained prompt reports unexpectedly low
/// provider cache reuse. The harness derives it from the original
/// [`SessionPromptCreated`] plus final [`ProviderResponseFinished`]
/// token usage so offline analysis can distinguish provider/cache-key
/// misses from obvious WS chain-strip misses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderCacheMissDiagnostic {
    /// Prompt id whose cache behavior looked unexpectedly low.
    pub session_prompt_id: SessionPromptId,
    /// Currently selected model as `"provider/model_id"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Previous provider response id the prompt attempted to chain from.
    pub previous_response_id: String,
    /// Expected provider-visible item index at which new prompt content began.
    pub previous_response_message_index: usize,
    /// Number of provider-visible prompt items that were expected to be
    /// cacheable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_prefix_count: Option<usize>,
    /// Prompt originator copied from the finished provider response.
    #[serde(default)]
    pub originator: PromptOriginator,
    /// Tool-choice mode used by the request that produced this diagnostic.
    #[serde(default, skip_serializing_if = "ToolChoice::is_default")]
    pub tool_choice: ToolChoice,
    /// Wire `prompt_cache_key` if known to the component emitting the
    /// diagnostic. The harness currently lacks provider config, so it
    /// leaves this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// WebSocket-pool turn delta, when the backend can report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_pool_delta: Option<WsPoolDelta>,
    /// Hex blake3 fingerprint of the provider-visible request fields
    /// Tau expects to remain stable across a chain.
    pub request_body_fingerprint: String,
    /// Input tokens reported by the current response.
    pub input_tokens: u64,
    /// Provider-cache-hit input tokens reported by the current response.
    pub cached_tokens: u64,
    /// Input tokens reported by the previous chained response.
    pub previous_input_tokens: u64,
    /// Estimated cacheable prefix tokens after correcting for request growth.
    pub cacheable_input_tokens: u64,
    /// Corrected cache-hit ratio for the cacheable prefix.
    pub corrected_cache_efficiency: f32,
}

/// Identifies the LLM backend that handled an
/// [`ProviderResponseFinished`].
///
/// Kind discriminates the provider API shape (Chat Completions vs.
/// Responses), and `base_url` pins down the specific endpoint —
/// `https://api.openai.com/v1` and `https://chatgpt.com/backend-api`
/// share the Responses kind but have very different cache /
/// rate-limit behavior, so the base URL is what an offline analysis
/// needs to tell them apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBackend {
    /// Provider API family used for the turn.
    pub kind: ProviderBackendKind,
    /// Base URL or origin of the upstream provider endpoint.
    pub base_url: String,
    /// Wire transport the turn was sent over. Defaults to
    /// `HttpSse` for backwards compatibility with sessions recorded
    /// before this field existed.
    #[serde(default)]
    pub transport: ProviderBackendTransport,
    /// The backend retried a rejected `previous_response_id` as a full replay.
    /// Surfaced here so the harness and offline tools can tell a successful
    /// response still paid the stale-chain recovery cost.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale_chain_fallback: bool,
}

/// The provider API shape an [`ProviderBackend`] talks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderBackendKind {
    ChatCompletions,
    Responses,
}

/// Transport the provider used to deliver one turn. `HttpSse` covers
/// both the Chat Completions path and the HTTP+SSE Responses path
/// (kind discriminates which API); `Websocket` is the Codex
/// Responses persistent-WS path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderBackendTransport {
    /// One-shot HTTP request with Server-Sent Events streaming.
    /// Default — covers Chat Completions and the HTTP+SSE Responses
    /// fallback.
    #[default]
    HttpSse,
    /// Persistent WebSocket. Only Codex Responses today.
    Websocket,
}

// ---------------------------------------------------------------------------
// Item-based conversation types
// ---------------------------------------------------------------------------

/// Role of a participant in one message item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRole {
    System,
    Developer,
    User,
    Assistant,
}

/// One content part inside a message item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
}

/// Opaque provider-owned payload preserved without interpretation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpaqueProviderItem(pub CborValue);

/// One message item in the prompt or assistant output timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageItem {
    pub role: ContextRole,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
}

/// One tool call item in the prompt or assistant output timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallItem {
    pub call_id: ToolCallId,
    pub name: ToolName,
    pub tool_type: ToolType,
    pub arguments: CborValue,
}

/// Terminal status for one tool result item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Success,
    Error { message: String },
    Cancelled { reason: String },
}

/// One rendered header in the text sent to a provider for a tool response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolResponseHeader {
    /// Header key rendered before the `: ` separator.
    pub key: String,
    /// Header value rendered after the `: ` separator.
    pub value: String,
}

/// Provider-facing text form of a tool response.
///
/// The canonical rendering is header lines in `<key>: <value>` form, followed
/// by an empty line and then the tool-specific body. Tool result events still
/// carry raw CBOR so extensions do not need to coordinate a wire-format
/// migration; this type is the normalized boundary used before provider output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResponse {
    /// Original tool payload kept for non-provider consumers that need
    /// structured data rather than rendered text.
    pub raw: CborValue,
    /// Structured headers rendered before the response body.
    pub headers: Vec<ToolResponseHeader>,
    /// Tool-specific response text rendered after the blank separator.
    pub body: String,
}

impl ToolResponse {
    /// Builds a normalized provider-facing response from a raw CBOR tool
    /// result.
    #[must_use]
    pub fn from_cbor(value: &CborValue) -> Self {
        match value {
            CborValue::Map(entries) => Self::from_cbor_map(entries),
            other => Self {
                raw: other.clone(),
                headers: Vec::new(),
                body: cbor_tool_response_text(other),
            },
        }
    }

    /// Renders this response as header lines, a blank line, then body text.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for header in &self.headers {
            out.push_str(&header.key);
            out.push_str(": ");
            out.push_str(&header.value);
            out.push('\n');
        }
        if !self.headers.is_empty() {
            out.push('\n');
        }
        out.push_str(&self.body);
        out
    }

    fn from_cbor_map(entries: &[(CborValue, CborValue)]) -> Self {
        let raw = CborValue::Map(entries.to_vec());
        let mut headers = Vec::new();
        let mut body_parts = Vec::new();
        for (key, value) in entries {
            let key = cbor_tool_response_text(key);
            let value = cbor_tool_response_text(value);
            if key == "output" || key == "line-numbered content" {
                body_parts.push(value);
            } else if value.contains('\n') {
                body_parts.push(format!("{key}:\n{value}"));
            } else {
                headers.push(ToolResponseHeader { key, value });
            }
        }
        Self {
            raw,
            headers,
            body: body_parts.join("\n"),
        }
    }
}

fn cbor_tool_response_text(value: &CborValue) -> String {
    match value {
        CborValue::Null => String::new(),
        CborValue::Bool(b) => b.to_string(),
        CborValue::Integer(i) => {
            let n: i128 = (*i).into();
            n.to_string()
        }
        CborValue::Float(f) => f.to_string(),
        CborValue::Text(s) => s.clone(),
        CborValue::Bytes(b) => format!("<{} bytes>", b.len()),
        CborValue::Array(arr) => arr
            .iter()
            .map(cbor_tool_response_text)
            .collect::<Vec<_>>()
            .join("\n"),
        CborValue::Map(entries) => ToolResponse::from_cbor_map(entries).render(),
        CborValue::Tag(_, inner) => cbor_tool_response_text(inner),
        _ => String::new(),
    }
}

/// One tool result item in the prompt timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultItem {
    pub call_id: ToolCallId,
    pub tool_type: ToolType,
    pub status: ToolResultStatus,
    pub output: ToolResponse,
}

/// One item in Tau's prompt/response timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ContextItem {
    Message(MessageItem),
    ToolCall(ToolCallItem),
    ToolResult(ToolResultItem),
    Reasoning(OpaqueProviderItem),
    Compaction(OpaqueProviderItem),
    UnknownProviderItem(OpaqueProviderItem),
}

/// Transcript node projected from durable facts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum TranscriptNode {
    UserInput(UserInputNode),
    AssistantResponse(AssistantResponseNode),
    ToolResults(ToolResultsNode),
    Compaction(CompactionNode),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserInputNode {
    pub items: Vec<ContextItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantResponseNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<ProviderBackend>,
    pub output_items: Vec<ContextItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderTokenUsage>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultsNode {
    pub items: Vec<ToolResultItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionNode {
    pub replacement_window: Vec<ContextItem>,
}

/// Assistant-message phase label, mirroring the OpenAI Codex
/// `phase` field on assistant `message` items.
///
/// The Codex Responses API attaches one of these to each assistant
/// turn it produces (on models that support it, currently
/// `gpt-5.3-codex` and later). Resending the same value on later
/// turns lets the model distinguish intermediate progress from
/// completed work — the doc-recommended remedy for "early stopping"
/// in long, tool-heavy runs.
///
/// We capture the value off the SSE stream, persist it alongside the
/// assistant turn, and echo it back on every re-serialized history
/// replay. Older models that do not emit this field still receive
/// the `final_answer` default on assistant message items the harness
/// re-serializes, which is the explicit guidance in the deployment
/// checklist.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    /// Intermediate progress / preliminary notes.
    Commentary,
    /// Final completed response.
    FinalAnswer,
}

impl MessagePhase {
    /// Wire string accepted by the OpenAI Codex Responses API on
    /// assistant `message` items.
    #[must_use]
    pub const fn as_openai_wire(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }
}

/// A tool definition available for the agent to use.
///
/// This is outbound (harness → LLM in the prompt), so the harness
/// controls the string and we enforce the `ToolName` invariant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: ToolName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_visible_name: Option<ToolName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether this is a JSON-schema function tool or a freeform custom tool.
    pub tool_type: ToolType,
    /// JSON Schema describing the tool's input parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Optional freeform/custom input format. `None` means provider-default
    /// unconstrained text for custom tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ToolFormat>,
}

// ---------------------------------------------------------------------------
// Top-level event envelope
// ---------------------------------------------------------------------------

/// Top-level event envelope used on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum Event {
    // Tools
    #[serde(rename = "tool.register")]
    ToolRegister(ToolRegister),
    #[serde(rename = "tool.unregister")]
    ToolUnregister(ToolUnregister),
    #[serde(rename = "tool.request")]
    ToolRequest(ToolRequest),
    #[serde(rename = "tool.invoke")]
    ToolInvoke(ToolInvoke),
    #[serde(rename = "tool.result")]
    ToolResult(ToolResult),
    #[serde(rename = "tool.error")]
    ToolError(ToolError),
    #[serde(rename = "tool.background_result")]
    ToolBackgroundResult(ToolBackgroundResult),
    #[serde(rename = "tool.background_error")]
    ToolBackgroundError(ToolBackgroundError),
    #[serde(rename = "tool.progress")]
    ToolProgress(ToolProgress),
    #[serde(rename = "tool.cancel")]
    ToolCancel(ToolCancel),
    #[serde(rename = "tool.cancelled")]
    ToolCancelled(ToolCancelled),
    #[serde(rename = "tool.delegate_progress")]
    ToolDelegateProgress(DelegateProgress),

    // Extension supervision
    #[serde(rename = "extension.starting")]
    ExtensionStarting(ExtensionStarting),
    #[serde(rename = "extension.ready")]
    ExtensionReady(ExtensionReady),
    #[serde(rename = "extension.exited")]
    ExtensionExited(ExtensionExited),
    #[serde(rename = "extension.restarting")]
    ExtensionRestarting(ExtensionRestarting),
    #[serde(rename = "extension.skill_available")]
    ExtSkillAvailable(ExtSkillAvailable),
    #[serde(rename = "extension.agents_md_available")]
    ExtAgentsMdAvailable(ExtAgentsMdAvailable),
    #[serde(rename = "extension.context_ready")]
    ExtensionContextReady(ExtensionContextReady),
    #[serde(rename = "extension.session_context_publish")]
    ExtSessionContextPublish(ExtSessionContextPublish),
    #[serde(rename = "extension.prompt_fragment_publish")]
    ExtPromptFragmentPublish(ExtPromptFragmentPublish),
    #[serde(rename = "extension.agent_query")]
    ExtAgentQuery(ExtAgentQuery),
    #[serde(rename = "extension.agent_query_result")]
    ExtAgentQueryResult(ExtAgentQueryResult),
    #[serde(rename = "extension.event")]
    ExtensionEvent(CustomEvent),
    #[serde(rename = "provider.models_updated")]
    ProviderModelsUpdated(ProviderModelsUpdated),

    // Harness info
    #[serde(rename = "harness.info")]
    HarnessInfo(HarnessInfo),
    #[serde(rename = "harness.session_dir")]
    HarnessSessionDir(HarnessSessionDir),
    #[serde(rename = "harness.ui_dir")]
    HarnessUiDir(HarnessUiDir),
    #[serde(rename = "harness.models_available")]
    HarnessModelsAvailable(HarnessModelsAvailable),
    #[serde(rename = "harness.roles_available")]
    HarnessRolesAvailable(HarnessRolesAvailable),
    #[serde(rename = "harness.role_selected")]
    HarnessRoleSelected(HarnessRoleSelected),
    #[serde(rename = "harness.context_usage_changed")]
    HarnessContextUsageChanged(HarnessContextUsageChanged),
    #[serde(rename = "harness.effort_changed")]
    HarnessEffortChanged(HarnessEffortChanged),
    #[serde(rename = "harness.service_tier_changed")]
    HarnessServiceTierChanged(HarnessServiceTierChanged),
    #[serde(rename = "harness.efforts_available")]
    HarnessEffortsAvailable(HarnessEffortsAvailable),
    #[serde(rename = "harness.verbosity_changed")]
    HarnessVerbosityChanged(HarnessVerbosityChanged),
    #[serde(rename = "harness.verbosities_available")]
    HarnessVerbositiesAvailable(HarnessVerbositiesAvailable),
    #[serde(rename = "harness.thinking_summary_changed")]
    HarnessThinkingSummaryChanged(HarnessThinkingSummaryChanged),
    #[serde(rename = "harness.thinking_summaries_available")]
    HarnessThinkingSummariesAvailable(HarnessThinkingSummariesAvailable),

    // UI
    #[serde(rename = "ui.prompt_submitted")]
    UiPromptSubmitted(UiPromptSubmitted),
    #[serde(rename = "ui.prompt_draft")]
    UiPromptDraft(UiPromptDraft),
    #[serde(rename = "ui.role_select")]
    UiRoleSelect(UiRoleSelect),
    #[serde(rename = "ui.role_update")]
    UiRoleUpdate(UiRoleUpdate),
    #[serde(rename = "ui.detach_request")]
    UiDetachRequest(UiDetachRequest),
    #[serde(rename = "ui.shell_command")]
    UiShellCommand(UiShellCommand),
    #[serde(rename = "ui.switch_session")]
    UiSwitchSession(UiSwitchSession),
    #[serde(rename = "ui.tree_request")]
    UiTreeRequest(UiTreeRequest),
    #[serde(rename = "ui.navigate_tree")]
    UiNavigateTree(UiNavigateTree),
    #[serde(rename = "ui.compact_request")]
    UiCompactRequest(UiCompactRequest),
    #[serde(rename = "ui.cancel_prompt")]
    UiCancelPrompt(UiCancelPrompt),
    #[serde(rename = "ui.recall_queued_prompt")]
    UiRecallQueuedPrompt(UiRecallQueuedPrompt),

    // Term (terminal-output side effects)
    #[serde(rename = "term.osc1337_set_user_var")]
    Osc1337SetUserVar(Osc1337SetUserVar),

    // Shell (user-initiated)
    #[serde(rename = "shell.command_progress")]
    ShellCommandProgress(ShellCommandProgress),
    #[serde(rename = "shell.command_finished")]
    ShellCommandFinished(ShellCommandFinished),

    // Session
    #[serde(rename = "session.prompt_queued")]
    SessionPromptQueued(SessionPromptQueued),
    #[serde(rename = "session.prompt_recalled")]
    SessionPromptRecalled(SessionPromptRecalled),
    #[serde(rename = "session.prompt_steered")]
    SessionPromptSteered(SessionPromptSteered),
    #[serde(rename = "session.started")]
    SessionStarted(SessionStarted),
    #[serde(rename = "session.shutdown")]
    SessionShutdown(SessionShutdown),
    #[serde(rename = "session.compaction_started")]
    SessionCompactionStarted(SessionCompactionStarted),
    #[serde(rename = "session.compaction_finished")]
    SessionCompactionFinished(SessionCompactionFinished),
    #[serde(rename = "session.compacted")]
    SessionCompacted(SessionCompacted),
    #[serde(rename = "session.compaction_requested")]
    SessionCompactionRequested(SessionCompactionRequested),
    #[serde(rename = "session.prompt_created")]
    SessionPromptCreated(SessionPromptCreated),
    #[serde(rename = "session.prompt_prewarm_requested")]
    SessionPromptPrewarmRequested(SessionPromptPrewarmRequested),
    #[serde(rename = "session.user_message_injected")]
    SessionUserMessageInjected(SessionUserMessageInjected),

    // Provider execution
    #[serde(rename = "provider.prompt_submitted")]
    ProviderPromptSubmitted(ProviderPromptSubmitted),
    #[serde(rename = "provider.response_updated")]
    ProviderResponseUpdated(ProviderResponseUpdated),
    #[serde(rename = "provider.response_finished")]
    ProviderResponseFinished(ProviderResponseFinished),
    #[serde(rename = "provider.cache_miss_diagnostic")]
    ProviderCacheMissDiagnostic(ProviderCacheMissDiagnostic),
}

impl Event {
    /// Returns the dotted event name carried by this envelope.
    #[must_use]
    pub fn name(&self) -> EventName {
        match self {
            Self::ToolRegister(_) => EventName::TOOL_REGISTER,
            Self::ToolUnregister(_) => EventName::TOOL_UNREGISTER,
            Self::ToolRequest(_) => EventName::TOOL_REQUEST,
            Self::ToolInvoke(_) => EventName::TOOL_INVOKE,
            Self::ToolResult(_) => EventName::TOOL_RESULT,
            Self::ToolError(_) => EventName::TOOL_ERROR,
            Self::ToolBackgroundResult(_) => EventName::TOOL_BACKGROUND_RESULT,
            Self::ToolBackgroundError(_) => EventName::TOOL_BACKGROUND_ERROR,
            Self::ToolProgress(_) => EventName::TOOL_PROGRESS,
            Self::ToolCancel(_) => EventName::TOOL_CANCEL,
            Self::ToolCancelled(_) => EventName::TOOL_CANCELLED,
            Self::ToolDelegateProgress(_) => EventName::TOOL_DELEGATE_PROGRESS,
            Self::ExtensionStarting(_) => EventName::EXTENSION_STARTING,
            Self::ExtensionReady(_) => EventName::EXTENSION_READY,
            Self::ExtensionExited(_) => EventName::EXTENSION_EXITED,
            Self::ExtensionRestarting(_) => EventName::EXTENSION_RESTARTING,
            Self::ExtSkillAvailable(_) => EventName::EXTENSION_SKILL_AVAILABLE,
            Self::ExtAgentsMdAvailable(_) => EventName::EXTENSION_AGENTS_MD_AVAILABLE,
            Self::ExtensionContextReady(_) => EventName::EXTENSION_CONTEXT_READY,
            Self::ExtSessionContextPublish(_) => EventName::EXTENSION_SESSION_CONTEXT_PUBLISH,
            Self::ExtPromptFragmentPublish(_) => EventName::EXTENSION_PROMPT_FRAGMENT_PUBLISH,
            Self::ExtAgentQuery(_) => EventName::EXTENSION_AGENT_QUERY,
            Self::ExtAgentQueryResult(_) => EventName::EXTENSION_AGENT_QUERY_RESULT,
            Self::ExtensionEvent(event) => event.name.clone(),
            Self::ProviderModelsUpdated(_) => EventName::PROVIDER_MODELS_UPDATED,
            Self::HarnessInfo(_) => EventName::HARNESS_INFO,
            Self::HarnessSessionDir(_) => EventName::HARNESS_SESSION_DIR,
            Self::HarnessUiDir(_) => EventName::HARNESS_UI_DIR,
            Self::HarnessModelsAvailable(_) => EventName::HARNESS_MODELS_AVAILABLE,
            Self::HarnessRolesAvailable(_) => EventName::HARNESS_ROLES_AVAILABLE,
            Self::HarnessRoleSelected(_) => EventName::HARNESS_ROLE_SELECTED,
            Self::HarnessContextUsageChanged(_) => EventName::HARNESS_CONTEXT_USAGE_CHANGED,
            Self::HarnessEffortChanged(_) => EventName::HARNESS_EFFORT_CHANGED,
            Self::HarnessServiceTierChanged(_) => EventName::HARNESS_SERVICE_TIER_CHANGED,
            Self::HarnessEffortsAvailable(_) => EventName::HARNESS_EFFORTS_AVAILABLE,
            Self::HarnessVerbosityChanged(_) => EventName::HARNESS_VERBOSITY_CHANGED,
            Self::HarnessVerbositiesAvailable(_) => EventName::HARNESS_VERBOSITIES_AVAILABLE,
            Self::HarnessThinkingSummaryChanged(_) => EventName::HARNESS_THINKING_SUMMARY_CHANGED,
            Self::HarnessThinkingSummariesAvailable(_) => {
                EventName::HARNESS_THINKING_SUMMARIES_AVAILABLE
            }
            Self::UiPromptSubmitted(_) => EventName::UI_PROMPT_SUBMITTED,
            Self::UiPromptDraft(_) => EventName::UI_PROMPT_DRAFT,
            Self::UiRoleSelect(_) => EventName::UI_ROLE_SELECT,
            Self::UiRoleUpdate(_) => EventName::UI_ROLE_UPDATE,
            Self::UiDetachRequest(_) => EventName::UI_DETACH_REQUEST,
            Self::UiShellCommand(_) => EventName::UI_SHELL_COMMAND,
            Self::UiSwitchSession(_) => EventName::UI_SWITCH_SESSION,
            Self::UiTreeRequest(_) => EventName::UI_TREE_REQUEST,
            Self::UiNavigateTree(_) => EventName::UI_NAVIGATE_TREE,
            Self::UiCompactRequest(_) => EventName::UI_COMPACT_REQUEST,
            Self::UiCancelPrompt(_) => EventName::UI_CANCEL_PROMPT,
            Self::UiRecallQueuedPrompt(_) => EventName::UI_RECALL_QUEUED_PROMPT,
            Self::Osc1337SetUserVar(_) => EventName::TERM_OSC1337_SET_USER_VAR,
            Self::ShellCommandProgress(_) => EventName::SHELL_COMMAND_PROGRESS,
            Self::ShellCommandFinished(_) => EventName::SHELL_COMMAND_FINISHED,
            Self::SessionPromptQueued(_) => EventName::SESSION_PROMPT_QUEUED,
            Self::SessionPromptRecalled(_) => EventName::SESSION_PROMPT_RECALLED,
            Self::SessionPromptSteered(_) => EventName::SESSION_PROMPT_STEERED,
            Self::SessionStarted(_) => EventName::SESSION_STARTED,
            Self::SessionShutdown(_) => EventName::SESSION_SHUTDOWN,
            Self::SessionCompactionStarted(_) => EventName::SESSION_COMPACTION_STARTED,
            Self::SessionCompactionFinished(_) => EventName::SESSION_COMPACTION_FINISHED,
            Self::SessionCompacted(_) => EventName::SESSION_COMPACTED,
            Self::SessionCompactionRequested(_) => EventName::SESSION_COMPACTION_REQUESTED,
            Self::SessionPromptCreated(_) => EventName::SESSION_PROMPT_CREATED,
            Self::SessionPromptPrewarmRequested(_) => EventName::SESSION_PROMPT_PREWARM_REQUESTED,
            Self::SessionUserMessageInjected(_) => EventName::SESSION_USER_MESSAGE_INJECTED,
            Self::ProviderPromptSubmitted(_) => EventName::PROVIDER_PROMPT_SUBMITTED,
            Self::ProviderResponseUpdated(_) => EventName::PROVIDER_RESPONSE_UPDATED,
            Self::ProviderResponseFinished(_) => EventName::PROVIDER_RESPONSE_FINISHED,
            Self::ProviderCacheMissDiagnostic(_) => EventName::PROVIDER_CACHE_MISS_DIAGNOSTIC,
        }
    }

    /// Returns true for protocol events that historically behaved as
    /// transient when sent directly without an [`crate::Emit`] wrapper.
    #[must_use]
    pub const fn defaults_to_transient(&self) -> bool {
        matches!(
            self,
            Self::ToolRequest(_)
                | Self::ToolError(_)
                | Self::ToolCancelled(_)
                | Self::ProviderResponseUpdated(_)
                | Self::ProviderPromptSubmitted(_)
                | Self::ToolProgress(_)
                | Self::ToolDelegateProgress(_)
                | Self::ShellCommandProgress(_)
                | Self::SessionPromptQueued(_)
                | Self::SessionPromptRecalled(_)
                | Self::SessionCompactionStarted(_)
                | Self::SessionCompactionFinished(_)
                | Self::SessionCompactionRequested(_)
                | Self::SessionPromptCreated(_)
                | Self::SessionPromptPrewarmRequested(_)
                | Self::UiCompactRequest(_)
                | Self::UiPromptDraft(_)
        )
    }
}
