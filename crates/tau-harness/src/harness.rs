//! [`Harness`]: the central event loop. Owns the bus, registry, session
//! store, and the live extensions; routes every event between the agent,
//! tools, and clients.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use tau_core::{
    ActionRegistry, Connection, ConnectionMetadata, ConnectionOrigin, DefaultSubscriptionPolicy,
    EventBus, PolicyStore, RouteError, SessionStore, SessionStoreError, ToolRegistry,
    ToolRouteError, ToolRouteTarget, validate_tool_arguments,
};
use tau_proto::{
    ActionError, ActionInvocationId, ActionInvoke, ActionResult, ActionSchemaPublished, AgentState,
    BackgroundSupport, CborValue, ClientKind, ContentPart, ContextItem, ContextRole, Disconnect,
    Event, EventSelector, ExtensionName, Frame, HarnessContextUsageChanged, HarnessRoleSelected,
    Message, MessageItem, ModelId, PreviousResponseCandidate, PromptFragment, PromptOriginator,
    ProviderCacheMissDiagnostic, ProviderModelInfo, ProviderResponseFinished, ProviderStopReason,
    ProviderTokenUsage, SessionAgentStateChanged, SessionCompactionRequested, SessionId,
    SessionPromptCreated, SessionPromptId, SessionPromptPrewarmRequested, SessionPromptQueued,
    SessionPromptRecalled, SessionPromptTerminated, SessionPromptTerminationReason,
    TokenUsageStats, ToolBackgroundError, ToolBackgroundResult, ToolCallId, ToolCallItem,
    ToolCancelled, ToolChoice, ToolDefinition, ToolError, ToolName, ToolRegister, ToolRejected,
    ToolRequest, ToolResult, ToolResultKind, ToolType, UiAgentStateAction, UiCancelPrompt,
};

use crate::conversation::{
    Conversation, ConversationId, ConversationTurnState, PendingCancel, PendingPrompt,
};
use crate::daemon::InteractionOutcome;
use crate::debug_log::DebugEventLog;
use crate::dedup::{
    DEFAULT_THRESHOLD_BYTES, build_pointer_error_message, build_pointer_value,
    encode_error_for_hash, encode_for_hash, hash_truncated,
};
use crate::dirs::policy_store_path_from;
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill, DiscoveredSkillSource};
use crate::error::HarnessError;
use crate::event::{
    ChannelSink, HarnessCommand, HarnessEvent, WriterShutdown, spawn_reader_thread,
    spawn_writer_thread,
};
use crate::event_log::EventLog;
#[cfg(any(test, feature = "echo-agent"))]
use crate::extension::spawn_in_process;
use crate::extension::{
    ExtensionConnectCommand, ExtensionEntry, ExtensionState, extension_stderr_log_path,
    spawn_supervised,
};
use crate::format::{format_tool_progress, render_entry_preview};
use crate::harness::interception::{
    ConversationHeadSync, DeferredPublish, InterceptorRegistry, PendingIntercept,
};
use crate::harness::subagents_tool::SubagentToolState;
use crate::internal_tools::InternalToolHandlers;
use crate::model::{
    LoadedRoles, MissingDefaultRole, baseline_params_for_selection, context_percent_used,
    context_window_for_model, efforts_for_model, fallback_role, load_roles, model_for_role,
    role_infos, select_model_for_role, selected_params_for_role, thinking_summaries_for_model,
    verbosities_for_model,
};
use crate::prompt::{
    BUILT_IN_SYSTEM_TEMPLATE_NAME, RolePromptTemplateContext, assemble_conversation_from,
    assemble_prompt_context_from, build_system_prompt_with_tool_template_context,
    built_in_system_prompt_templates, render_agents_context_message,
};
use crate::secrets::{load_secret_sources, resolve_extension_secrets};
use crate::settings::{Config, load_harness_settings_or_warn};
use crate::tool_turn::{ForegroundAction, PendingToolInvocation, ToolTurnMachine};
use crate::turn::{PromptSubmission, TurnState};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const AUTO_COMPACTION_CONTEXT_PERCENT: u8 = 90;
const BUILT_IN_SKILLS_SOURCE_ID: &str = "harness:built-in-skills";
const SELF_KNOWLEDGE_VERSION_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_VERSION__";
const SELF_KNOWLEDGE_HASH_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_HASH__";
const SELF_KNOWLEDGE_BUILD_DATE_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_BUILD_DATE__";
const SELF_KNOWLEDGE_HARNESS_CONFIG_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_HARNESS_CONFIG__";
const SELF_KNOWLEDGE_UI_CONFIG_TOKEN: &str = "__TAU_SELF_KNOWLEDGE_UI_CONFIG__";
const SELF_KNOWLEDGE_HARNESS_CONFIG: &str =
    include_str!("../../tau-config/config/built-in.harness.yaml");
const SELF_KNOWLEDGE_UI_CONFIG: &str = include_str!("../../tau-config/config/built-in.cli.yaml");

pub(crate) fn background_completion_prompt(call_id: &ToolCallId) -> String {
    format!(
        "{} Tool call `{call_id}` is complete.",
        crate::INTERNAL_MARKER
    )
}

const RESTORE_NOTICE_BODY_PREFIX: &str = "Previous session was interrupted and restored.";

/// Text for the one-shot model-visible notice folded into the first user turn
/// after a cold session resume.
pub(crate) fn restore_notice_prompt(
    last_recorded_at: Option<tau_proto::UnixMicros>,
    now: tau_proto::UnixMicros,
) -> String {
    restore_notice_prompt_for_elapsed_inner(restore_notice_elapsed(last_recorded_at, now))
}

/// Test helper that formats the restore notice for a fixed elapsed duration.
#[cfg(test)]
pub(crate) fn restore_notice_prompt_for_elapsed(elapsed: Option<Duration>) -> String {
    restore_notice_prompt_for_elapsed_inner(elapsed)
}

fn restore_notice_prompt_for_elapsed_inner(elapsed: Option<Duration>) -> String {
    let timing = elapsed.map_or_else(
        || "The state of the world might have changed since the last session.".to_owned(),
        |elapsed| {
            format!(
                "{} since the last recorded session event, and the state of the world might have changed.",
                format_restore_notice_elapsed(elapsed)
            )
        },
    );
    format!(
        "{} {RESTORE_NOTICE_BODY_PREFIX} {timing}",
        crate::INTERNAL_MARKER
    )
}

fn restore_notice_elapsed(
    last_recorded_at: Option<tau_proto::UnixMicros>,
    now: tau_proto::UnixMicros,
) -> Option<Duration> {
    let last = last_recorded_at?;
    if last.get() == 0 || now.get() < last.get() {
        return None;
    }
    Some(Duration::from_micros(now.get() - last.get()))
}

fn format_restore_notice_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return "Less than 1 minute has passed".to_owned();
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format_elapsed_count(minutes, "minute");
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format_elapsed_count(hours, "hour");
    }

    format_elapsed_count(hours / 24, "day")
}

fn format_elapsed_count(count: u64, unit: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    let verb = if count == 1 { "has" } else { "have" };
    format!("{count} {unit}{suffix} {verb} passed")
}

/// Returns true when `text` is the hidden one-shot restore notice.
pub(crate) fn is_restore_notice_prompt_text(text: &str) -> bool {
    text.strip_prefix(crate::INTERNAL_MARKER)
        .and_then(|text| text.strip_prefix(" "))
        .is_some_and(|text| text.starts_with(RESTORE_NOTICE_BODY_PREFIX))
}

fn event_is_internal_prompt_text(event: &Event, text: &str) -> bool {
    match event {
        Event::UiPromptSubmitted(prompt) => {
            prompt.message_class.is_internal() && prompt.text == text
        }
        Event::SessionPromptSteered(steered) => {
            steered.message_class.is_internal() && steered.text == text
        }
        Event::SessionUserMessageInjected(injected) => {
            injected.message_class.is_internal() && injected.text == text
        }
        _ => false,
    }
}

fn event_is_internal_restore_notice(event: &Event) -> bool {
    match event {
        Event::UiPromptSubmitted(prompt) => {
            prompt.message_class.is_internal() && is_restore_notice_prompt_text(&prompt.text)
        }
        Event::SessionPromptSteered(steered) => {
            steered.message_class.is_internal() && is_restore_notice_prompt_text(&steered.text)
        }
        Event::SessionUserMessageInjected(injected) => {
            injected.message_class.is_internal() && is_restore_notice_prompt_text(&injected.text)
        }
        _ => false,
    }
}

fn restored_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn restored_background_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted due to session restart. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn extension_disconnected_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nTool call `{call_id}` was interrupted because extension disconnected. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

fn extension_disconnected_background_tool_call_error_message(call_id: &ToolCallId) -> String {
    format!(
        "{}: true\n\nBackground tool call `{call_id}` was interrupted because extension disconnected. Side effects may have occurred.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

/// Model-visible internal tool error for calls whose provider is no longer
/// live.
pub(crate) fn unavailable_tool_error_message(tool_name: &ToolName) -> String {
    format!(
        "{}: true\n\nTool `{tool_name}` is not available.",
        tau_proto::TAU_INTERNAL_HEADER_NAME
    )
}

/// Hidden prompt text used to tell the model a tool left the live registry.
pub(crate) fn tool_unavailable_notice_prompt(tool_name: &ToolName) -> String {
    format!(
        "{} Tool `{tool_name}` is temporarily no longer available.",
        crate::INTERNAL_MARKER
    )
}

/// Hidden prompt text used to tell the model a previously missing tool
/// returned.
pub(crate) fn tool_available_again_notice_prompt(tool_name: &ToolName) -> String {
    format!(
        "{} Tool `{tool_name}` is available again.",
        crate::INTERNAL_MARKER
    )
}

fn remove_pending_internal_prompt_text(prompts: &mut VecDeque<PendingPrompt>, text: &str) -> bool {
    let before = prompts.len();
    prompts.retain(|prompt| !(prompt.is_internal() && prompt.text == text));
    prompts.len() != before
}

fn load_system_prompt_templates(config_dir: Option<&Path>) -> HashMap<String, String> {
    let mut templates = built_in_system_prompt_templates();
    let Some(config_dir) = config_dir else {
        return templates;
    };
    let prompts_dir = config_dir.join("prompts");
    let Ok(entries) = std::fs::read_dir(prompts_dir) else {
        return templates;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("hbs") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                templates.insert(name.to_owned(), content);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "failed to read prompt template");
            }
        }
    }
    templates
}

#[derive(Clone, Debug)]
struct SessionContextContribution {
    extension_name: String,
    value: tau_proto::SessionContextValue,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SessionContextStore {
    by_session: BTreeMap<
        SessionId,
        BTreeMap<
            tau_proto::SessionContextKey,
            BTreeMap<tau_proto::ConnectionId, SessionContextContribution>,
        >,
    >,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PromptFragmentSource {
    RoleConfig {
        role_name: String,
    },
    Extension {
        connection_id: tau_proto::ConnectionId,
    },
    Tool {
        connection_id: tau_proto::ConnectionId,
    },
}

impl PromptFragmentSource {
    fn sort_key(&self) -> (&str, u8) {
        match self {
            // Role-config fragments have no extension connection id. Keep them
            // deterministic without pretending they came from a magic string
            // connection.
            Self::RoleConfig { role_name } => (role_name.as_str(), 0),
            Self::Extension { connection_id } => (connection_id.as_str(), 1),
            Self::Tool { connection_id } => (connection_id.as_str(), 2),
        }
    }
}

#[derive(Clone, Debug)]
struct SourcedPromptFragment {
    source: PromptFragmentSource,
    fragment: PromptFragment,
}

fn sort_sourced_prompt_fragments(fragments: &mut [SourcedPromptFragment]) {
    fragments.sort_by(|a, b| {
        a.fragment
            .priority
            .cmp(&b.fragment.priority)
            .then_with(|| a.source.sort_key().cmp(&b.source.sort_key()))
            .then_with(|| a.fragment.name.cmp(&b.fragment.name))
    });
}

fn sorted_prompt_fragments(
    fragments: impl IntoIterator<Item = SourcedPromptFragment>,
) -> Vec<PromptFragment> {
    let mut fragments = fragments.into_iter().collect::<Vec<_>>();
    sort_sourced_prompt_fragments(&mut fragments);
    fragments
        .into_iter()
        .map(|sourced| sourced.fragment)
        .collect()
}

impl SessionContextStore {
    /// Store or replace one contributor's value for a session context key.
    pub(crate) fn publish(
        &mut self,
        session_id: SessionId,
        key: tau_proto::SessionContextKey,
        contributor: tau_proto::ConnectionId,
        extension_name: String,
        value: tau_proto::SessionContextValue,
    ) {
        self.by_session
            .entry(session_id)
            .or_default()
            .entry(key)
            .or_default()
            .insert(
                contributor,
                SessionContextContribution {
                    extension_name,
                    value,
                },
            );
    }

    /// Return the Handlebars-visible `session_context` object for one session.
    pub(crate) fn template_value(&self, session_id: &SessionId) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        let Some(keys) = self.by_session.get(session_id) else {
            return serde_json::Value::Object(object);
        };
        for (key, contributions) in keys {
            let mut wrappers: Vec<_> = contributions
                .iter()
                .map(|(connection_id, contribution)| {
                    (
                        contribution.extension_name.clone(),
                        connection_id.clone(),
                        serde_json::json!({
                            "extension_name": contribution.extension_name,
                            "value": contribution.value.0,
                        }),
                    )
                })
                .collect();
            wrappers.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            object.insert(
                key.to_string(),
                serde_json::Value::Array(wrappers.into_iter().map(|(_, _, value)| value).collect()),
            );
        }
        serde_json::Value::Object(object)
    }
}

#[derive(Clone, Debug)]
pub struct AgentToolCall {
    /// Provider-supplied tool call id.
    pub id: ToolCallId,
    /// Internal tool name selected by routing.
    pub name: ToolName,
    /// Protocol tool type.
    pub tool_type: tau_proto::ToolType,
    /// CBOR arguments supplied by the model/provider.
    pub arguments: CborValue,
    /// Optional provider display hint.
    pub display: Option<tau_proto::ToolDisplay>,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTool {
    pub(crate) name: ToolName,
    pub(crate) internal_name: ToolName,
    pub(crate) tool_type: ToolType,
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

const AGENT_ID_SUFFIX_BYTES: usize = 2;
const AGENT_ID_SUFFIX_MAX_ATTEMPTS: usize = 4096;

fn random_agent_id_suffix() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; AGENT_ID_SUFFIX_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_bytes(&bytes)
}

fn mint_available_agent_id_for_role_with(
    role: &str,
    mut is_taken: impl FnMut(&str) -> bool,
    mut next_suffix: impl FnMut() -> String,
) -> String {
    let mut attempts = 0;
    while attempts < AGENT_ID_SUFFIX_MAX_ATTEMPTS {
        let agent_id = format!("{role}_{}", next_suffix());
        if !is_taken(&agent_id) {
            return agent_id;
        }
        attempts += 1;
    }
    panic!(
        "unable to mint unique agent id for role `{role}` after {AGENT_ID_SUFFIX_MAX_ATTEMPTS} attempts"
    );
}

#[cfg(test)]
fn mint_agent_id_for_role(role: &str) -> String {
    mint_available_agent_id_for_role_with(role, |_| false, random_agent_id_suffix)
}

fn built_in_discovered_skills() -> HashMap<tau_proto::SkillName, DiscoveredSkill> {
    tau_skills::built_in_skills()
        .into_iter()
        .map(|skill| {
            (
                tau_proto::SkillName::from(skill.name),
                DiscoveredSkill {
                    source_id: BUILT_IN_SKILLS_SOURCE_ID.into(),
                    description: skill.description,
                    source: DiscoveredSkillSource::BuiltIn {
                        content: render_self_knowledge_content(skill.content),
                    },
                    add_to_prompt: skill.add_to_prompt,
                },
            )
        })
        .collect()
}

fn render_self_knowledge_content(
    content: std::borrow::Cow<'static, str>,
) -> std::borrow::Cow<'static, str> {
    let last_modified = crate::version::build_last_modified().unwrap_or_else(|| "unknown".into());
    std::borrow::Cow::Owned(
        content
            .replace(SELF_KNOWLEDGE_VERSION_TOKEN, env!("CARGO_PKG_VERSION"))
            .replace(SELF_KNOWLEDGE_HASH_TOKEN, &crate::version::build_revision())
            .replace(SELF_KNOWLEDGE_BUILD_DATE_TOKEN, &last_modified)
            .replace(
                SELF_KNOWLEDGE_HARNESS_CONFIG_TOKEN,
                SELF_KNOWLEDGE_HARNESS_CONFIG,
            )
            .replace(SELF_KNOWLEDGE_UI_CONFIG_TOKEN, SELF_KNOWLEDGE_UI_CONFIG),
    )
}

pub(crate) fn assistant_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
    let text = output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content,
                ..
            }) => Some(
                content
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => text.as_str(),
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

pub(crate) fn tool_calls_from_output_items(output_items: &[ContextItem]) -> Vec<AgentToolCall> {
    output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(AgentToolCall {
                id: call.call_id.clone(),
                name: call.name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                display: None,
            }),
            _ => None,
        })
        .collect()
}

fn response_requests_tool_calls(response: &ProviderResponseFinished) -> bool {
    if response.stop_reason.requests_tool_calls() {
        return true;
    }
    if response.stop_reason != ProviderStopReason::EndTurn {
        return false;
    }
    response
        .output_items
        .iter()
        .any(|item| matches!(item, ContextItem::ToolCall(_)))
}

fn compaction_items_from_output_items(output_items: &[ContextItem]) -> Vec<ContextItem> {
    output_items
        .iter()
        .filter(|item| matches!(item, ContextItem::Compaction(_)))
        .cloned()
        .collect()
}

/// Estimate how many prompt/input tokens the compacted replacement window will
/// occupy when replayed on the next turn.
///
/// Tau does not carry a tokenizer in the harness, and the OpenAI standalone
/// compaction endpoint does not normally report token usage. For UI status we
/// use the same coarse convention used by many provider dashboards: roughly
/// four UTF-8 bytes per token, measured over the provider-owned item payloads
/// that prompt assembly will replay after compaction. This is not a billing
/// counter; it is a prompt-size estimate for the `compact … ok: #…` chip.
fn estimate_compacted_input_tokens(replacement_window: &[ContextItem]) -> Option<u64> {
    const APPROX_BYTES_PER_TOKEN: u64 = 4;

    let bytes: u64 = replacement_window
        .iter()
        .map(approx_context_item_provider_bytes)
        .sum();
    (bytes > 0).then_some(bytes.div_ceil(APPROX_BYTES_PER_TOKEN).max(1))
}

fn approx_context_item_provider_bytes(item: &ContextItem) -> u64 {
    match item {
        ContextItem::Message(message) => {
            let content_bytes: u64 = message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => text.len() as u64,
                })
                .sum();
            // Small role/item overhead keeps tiny summaries from looking free
            // without dominating real summaries.
            content_bytes + 16
        }
        ContextItem::ToolCall(call) => {
            call.call_id.as_str().len() as u64
                + call.name.as_str().len() as u64
                + approx_cbor_json_bytes(&call.arguments)
                + 16
        }
        ContextItem::ToolResult(result) => {
            let status_bytes = match &result.status {
                tau_proto::ToolResultStatus::Success => 0,
                tau_proto::ToolResultStatus::Error { message }
                | tau_proto::ToolResultStatus::Cancelled { reason: message } => {
                    message.len() as u64
                }
            };
            result.call_id.as_str().len() as u64
                + status_bytes
                + result.output.render().len() as u64
                + 16
        }
        ContextItem::Reasoning(item)
        | ContextItem::Compaction(item)
        | ContextItem::UnknownProviderItem(item) => approx_cbor_json_bytes(&item.0),
    }
}

fn approx_cbor_json_bytes(value: &CborValue) -> u64 {
    match value {
        CborValue::Null => 4,
        CborValue::Bool(value) => {
            if *value {
                4
            } else {
                5
            }
        }
        CborValue::Integer(value) => {
            let value: i128 = (*value).into();
            value.to_string().len() as u64
        }
        CborValue::Float(value) => value.to_string().len() as u64,
        CborValue::Bytes(bytes) => (bytes.len() as u64).div_ceil(3) * 4,
        CborValue::Text(text) => text.len() as u64,
        CborValue::Array(values) => {
            2 + values.iter().map(approx_cbor_json_bytes).sum::<u64>()
                + values.len().saturating_sub(1) as u64
        }
        CborValue::Map(entries) => {
            2 + entries
                .iter()
                .map(|(key, value)| approx_cbor_json_bytes(key) + approx_cbor_json_bytes(value) + 3)
                .sum::<u64>()
                + entries.len().saturating_sub(1) as u64
        }
        CborValue::Tag(_, value) => approx_cbor_json_bytes(value),
        _ => 0,
    }
}

#[cfg(test)]
mod tests;

mod dispatch;
mod interception;
mod replay;
#[allow(dead_code)]
mod skill_tool;
mod subagents_tool;

/// Connection ID used for harness-owned tools and their side-query
/// [`PromptOriginator`] name (e.g. `skill`, `delegate`, and `wait`).
pub(crate) const HARNESS_CONNECTION_ID: &str = "__harness__";

#[derive(Clone, Debug)]
pub(crate) struct PromptCacheDiagnosticContext {
    pub(crate) model: Option<ModelId>,
    pub(crate) previous_response: Option<PreviousResponseCandidate>,
    pub(crate) originator: PromptOriginator,
    pub(crate) tool_choice: ToolChoice,
    pub(crate) request_fingerprint: [u8; 32],
}

#[derive(Debug, Default)]
pub(crate) struct CurrentSessionState {
    /// Input tokens consumed by the most recent agent response, if
    /// the provider reported it. `None` until the first usage report
    /// for the current model.
    pub(crate) context_input_tokens: Option<u64>,
    /// Cached input tokens consumed by the most recent agent
    /// response, if the provider reported them.
    pub(crate) context_cached_tokens: Option<u64>,
    /// Percentage of the selected model's context window currently
    /// used. `None` when the model's context window is unknown.
    pub(crate) context_percent_used: Option<u8>,
    /// Current-session token usage totals.
    pub(crate) token_usage: TokenUsageStats,
}

#[derive(Debug)]
enum PendingCompactionResume {
    UserPrompt(String),
    FollowupTurn,
    None,
}

#[derive(Debug)]
struct PendingCompaction {
    target_cid: ConversationId,
    session_id: SessionId,
    target_agent_id: Option<String>,
    originator: PromptOriginator,
    original_input_tokens: Option<u64>,
    resume: PendingCompactionResume,
}

#[derive(Debug)]
struct PendingStartAgentRequest {
    source_id: String,
    extension_name: String,
    query: tau_proto::StartAgentRequest,
    role: String,
    cid: ConversationId,
    parent_cid: ConversationId,
    agent_id: String,
    pending_agent_messages: VecDeque<PendingPrompt>,
}

#[derive(Debug)]
struct ActiveStartAgentRequest {
    execution_mode: tau_proto::ToolExecutionMode,
}

#[derive(Clone, Debug)]
struct PendingActionInvocation {
    provider_connection_id: tau_proto::ConnectionId,
    requester_client_id: tau_proto::ConnectionId,
    action_id: String,
}

#[derive(Clone, Debug)]
struct StagedExtensionPublish {
    /// Event payload withheld until the source extension reaches `Ready`.
    event: Event,
    /// Whether the staged event should skip durable session history.
    transient: bool,
}

#[derive(Clone, Debug, Default)]
struct ExtensionActivationStage {
    /// Tool registrations received before the extension finished its handshake.
    tool_registrations: Vec<ToolRegister>,
    /// Provider model snapshots received before `Ready`, in wire order.
    provider_model_updates: Vec<tau_proto::ProviderModelsUpdated>,
    /// Action schema received before `Ready`. Schema publishing is a
    /// replacement, so only the latest staged schema matters.
    action_schema: Option<tau_actions::ActionSchema>,
    /// Skill announcements received before `Ready`, in wire order.
    skill_announcements: Vec<tau_proto::ExtSkillAvailable>,
    /// AGENTS.md announcements received before `Ready`, in wire order.
    agents_files: Vec<tau_proto::ExtAgentsMdAvailable>,
    /// Session context publishes received before `Ready`, in wire order.
    session_context_publishes: Vec<tau_proto::ExtSessionContextPublish>,
    /// Extension-level prompt fragments received before `Ready`, keyed by name
    /// so repeated publishes replace earlier staged content.
    prompt_fragments: BTreeMap<String, PromptFragment>,
    /// Interceptor registration received before `Ready`. Registration is a
    /// replacement, so only the latest staged message matters.
    intercept: Option<tau_proto::Intercept>,
    /// Session-init acknowledgements received before `Ready`, in wire order.
    context_ready_events: Vec<tau_proto::ExtensionContextReady>,
    /// Extension-started agent queries received before `Ready`, in wire order.
    agent_queries: Vec<tau_proto::StartAgentRequest>,
    /// Generic extension emits/events withheld until `Ready`.
    emitted_events: Vec<StagedExtensionPublish>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PendingToolAvailabilityNotice {
    Unavailable { visible_name: ToolName },
    AvailableAgain { visible_name: ToolName },
}

impl PendingToolAvailabilityNotice {
    fn prompt_text(&self) -> String {
        match self {
            Self::Unavailable { visible_name } => tool_unavailable_notice_prompt(visible_name),
            Self::AvailableAgain { visible_name } => {
                tool_available_again_notice_prompt(visible_name)
            }
        }
    }
}

/// Message recipient state used to report precise tool errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentMessageRecipientStatus {
    /// The recipient is known and can receive messages now.
    Live,
    /// The recipient id was known earlier but its agent has stopped.
    Stopped,
    /// The recipient id has never been observed by this harness.
    Unknown,
}

pub struct Harness {
    /// Sender side of the harness's central event channel. Cloned into
    /// each per-connection reader thread so they can feed
    /// `HarnessEvent`s back into the main loop.
    pub(crate) tx: Sender<HarnessEvent>,
    /// Receiver side of the central event channel. The main loop
    /// blocks on this and dispatches one `HarnessEvent` at a time.
    pub(crate) rx: Receiver<HarnessEvent>,
    /// Routes protocol events between connections (agent ↔ extensions
    /// ↔ socket clients). Owns connection state and per-connection
    /// outgoing queues.
    pub(crate) bus: EventBus,
    /// Maps tool name → providing connection. Used to resolve
    /// `ToolRequest` into either a broadcast `ToolStarted`
    /// or a broadcast `ToolRejected`.
    pub(crate) registry: ToolRegistry,
    /// Maps extension-provided UI actions to their owning extension connection.
    pub(crate) action_registry: ActionRegistry,
    /// Injected handlers for tools implemented inside the harness process.
    pub(crate) internal_tool_handlers: InternalToolHandlers,
    /// Runtime state root for this harness. Extension-specific persistent
    /// directories are allocated below this path and sent in Configure.
    pub(crate) state_dir: PathBuf,
    /// Append-only on-disk session store. Owns one `SessionTree` per
    /// session id, derived by folding the durable per-session event log
    /// at `<state_dir>/<session_id>/events.cbor`. The tree is never
    /// mutated through any other path — every entry comes from a
    /// persisted protocol event, so the on-disk log and the in-memory
    /// view cannot drift.
    pub(crate) store: SessionStore,
    /// The single session this harness owns. UserMessages with a
    /// different `session_id` are rejected. Pi-style: one harness =
    /// one active session at a time. Switching sessions tears the
    /// harness down and respawns extensions; that's a future
    /// `switch_session` operation, not silent multi-session.
    pub(crate) current_session_id: SessionId,
    /// `call_id` → owning conversation for every tool call currently
    /// in flight. Read by `session_id_for_event` (via the
    /// conversation) to attribute incoming `ToolResult` / `ToolError`
    /// / `ToolProgress` events back to the originating session.
    pub(crate) tool_conversations: std::collections::HashMap<ToolCallId, ConversationId>,
    /// `call_id` → pending tool metadata for in-flight calls. Used to
    /// enrich terminal runtime events before they are folded into
    /// durable transcript facts.
    pub(crate) pending_tools: std::collections::HashMap<ToolCallId, PendingTool>,
    /// Tool call ids that were known to this harness and reached a terminal
    /// state. Used to distinguish completed calls from typos in user-facing
    /// cancellation errors.
    pub(crate) completed_tool_calls: std::collections::HashSet<ToolCallId>,
    /// `call_id` → connection id of the extension currently servicing
    /// the call. Needed to route cancellation requests back to the
    /// right provider.
    pub(crate) pending_tool_providers:
        std::collections::HashMap<ToolCallId, tau_proto::ConnectionId>,
    /// `invocation_id` → action provider/requester pair for UI-directed
    /// action result routing and source validation.
    pending_action_invocations: HashMap<ActionInvocationId, PendingActionInvocation>,
    /// Append-only ring of recent protocol events. Client follower
    /// threads tail this log on connect to replay state and stay live.
    pub(crate) event_log: std::sync::Arc<EventLog>,
    /// Writer channels for socket clients, keyed by connection ID.
    /// Used to start follower threads for log-based replay + delivery.
    pub(crate) client_writers: std::collections::HashMap<tau_proto::ConnectionId, Sender<Frame>>,
    /// Buffered human-readable lifecycle messages (extension init,
    /// model changes, etc.) surfaced to the UI as part of the next
    /// `InteractionOutcome`.
    pub(crate) lifecycle_messages: Vec<String>,
    /// Every spawned or in-process extension, keyed by current
    /// `ConnectionId`. Supervises restart, shutdown, and per-extension
    /// ack state. Lookups by connection id (the hot per-event path —
    /// every `Ack`, `Hello`, `Ready`, `Disconnected`) are O(1).
    pub(crate) extensions: std::collections::HashMap<tau_proto::ConnectionId, ExtensionEntry>,
    /// Extension-originated state announced during handshake and withheld until
    /// the extension sends `Ready`. Activation happens in the main harness loop
    /// so prompt assembly, routing, and subscribers see the full batch at once.
    extension_activation_staging:
        std::collections::HashMap<tau_proto::ConnectionId, ExtensionActivationStage>,
    /// Spawn-order list of connection ids into `extensions`. Drives
    /// the deterministic "start every extension" and shutdown loops
    /// that a `HashMap` alone can't supply, and is updated in place
    /// whenever a supervised extension respawns with a fresh id.
    pub(crate) extension_order: Vec<tau_proto::ConnectionId>,
    /// Number of queued extension connect commands not yet applied by
    /// the harness loop. Startup waits on this before treating an empty
    /// `extensions` map as ready.
    pending_extension_connects: usize,
    /// Monotonic counter used to mint synthetic `sp-N`
    /// `SessionPromptId`s when dispatching prompts to the agent.
    pub(crate) next_session_prompt_id: u64,
    /// Maps session_prompt_id → owning conversation for in-flight
    /// prompts. The conversation knows its `session_id`, so older
    /// `prompt_sessions[spid]` lookups become two hops:
    /// `prompt_conversations[spid]` → `conversations[cid].session_id`.
    pub(crate) prompt_conversations: std::collections::HashMap<SessionPromptId, ConversationId>,
    /// Materialized full `session.prompt_created` payloads by id.
    /// New prompts are emitted fully materialized; snapshots remain so
    /// late joiners and legacy `tools_ref` events can still be served.
    pub(crate) prompt_snapshots: std::collections::HashMap<SessionPromptId, SessionPromptCreated>,
    /// Per-prompt fields needed to explain a low provider cache hit
    /// after the final usage report arrives. Kept outside
    /// `prompt_snapshots` because diagnostics need derived send-time
    /// metadata like the previous-response candidate.
    pub(crate) prompt_cache_diagnostics:
        std::collections::HashMap<SessionPromptId, PromptCacheDiagnosticContext>,
    /// All in-flight conversations keyed by `ConversationId`. The
    /// user's interactive UI thread is one fixed entry (see
    /// `default_conversation_id`); side queries from extensions spawn
    /// additional entries that live until their final response is
    /// routed back to the requesting extension.
    pub(crate) conversations: std::collections::HashMap<ConversationId, Conversation>,
    /// Agent id to conversation routing for addressable agents in the current
    /// session. Suspended agents remain here so `/agent resume` and follow-up
    /// prompts can continue their conversation.
    pub(crate) agent_conversations: HashMap<String, ConversationId>,
    /// Harness-owned lifecycle state for current-session agents.
    pub(crate) agent_states: HashMap<String, AgentState>,
    /// Agent ids that were once known but can no longer receive messages.
    pub(crate) stopped_agent_ids: HashSet<String>,
    /// Id of the user's main interactive conversation. Always present
    /// in `conversations` for the harness's whole lifetime.
    pub(crate) default_conversation_id: ConversationId,
    /// Global harness state. Currently only tracks per-session init
    /// (waiting on extensions to announce skills + AGENTS.md). Agent
    /// turn state is per-conversation; multiple conversations may have
    /// in-flight prompts simultaneously and the agent extension
    /// serializes its own consumption of `SessionPromptCreated`.
    pub(crate) turn_state: TurnState,
    /// Append-only event debug log.
    pub(crate) debug_log: Option<DebugEventLog>,
    /// Event emission interceptors, exact name first and prefix fallback.
    pub(crate) interceptors: InterceptorRegistry,
    /// Currently in-flight interception. While `Some(_)`, no new
    /// publishes commit — they queue onto `deferred_publishes` until
    /// the awaited [`InterceptReply`] arrives (or the awaited
    /// connection disconnects, treated as `Pass(None)`).
    pub(crate) pending_intercept: Option<PendingIntercept>,
    /// Publishes that arrived while `pending_intercept` was active.
    /// Drained in FIFO order once the pending intercept resolves.
    pub(crate) deferred_publishes: VecDeque<DeferredPublish>,
    /// Conversations whose just-published `UiPromptSubmitted` (or
    /// equivalent user-message event) has not yet committed because
    /// it is parked in the interception chain. Each entry triggers
    /// a `send_prompt_to_agent_for` call once the next
    /// user-message-bearing event commits — that's when the
    /// `SessionTree` reflects the prompt and the assembled message
    /// list will actually contain it. Without this, the agent
    /// receives a stale message list (the "Ready" loop bug). Owned by
    /// the defer/dispatch helpers in `harness::interception`.
    pub(crate) pending_user_prompt_dispatches: VecDeque<ConversationId>,
    /// Conversations whose next agent prompt is ready except that an
    /// unrelated publish is still parked in the interception chain.
    /// These do not wait for another user-message fold; they drain
    /// once interception and deferred publishes are idle. Owned by the
    /// defer/dispatch helpers in `harness::interception`.
    pub(crate) pending_publish_idle_dispatches: VecDeque<ConversationId>,
    /// All available models.
    pub(crate) available_models: Vec<ModelId>,
    /// Model snapshots published by provider extensions, keyed by sender
    /// connection.
    pub(crate) provider_models_by_extension: HashMap<String, Vec<ProviderModelInfo>>,
    /// Flattened provider model metadata keyed by model id. Rebuilt from
    /// [`Self::provider_models_by_extension`] whenever a provider snapshot
    /// changes.
    pub(crate) provider_model_info: HashMap<ModelId, ProviderModelInfo>,
    /// Provider extension connection for each model id. This is kept alongside
    /// [`Self::provider_model_info`] so prompt routing can address the provider
    /// that most recently published the selected model.
    pub(crate) provider_model_routes: HashMap<ModelId, tau_proto::ConnectionId>,
    /// Provider connection that received each in-flight prompt request.
    /// Incoming provider execution events must match this owner before the
    /// harness will publish streaming updates or accept the final response.
    pub(crate) pending_provider_prompts: HashMap<SessionPromptId, tau_proto::ConnectionId>,
    /// Available agent roles.
    pub(crate) available_roles: std::collections::HashMap<String, tau_config::settings::AgentRole>,
    /// Ordered role navigation groups for the currently available roles.
    pub(crate) available_role_groups: Vec<tau_proto::HarnessRoleGroup>,
    /// Role overrides changed at runtime for this process.
    pub(crate) role_overrides: std::collections::HashMap<String, tau_config::settings::AgentRole>,
    /// Currently selected role. The resolved model is derived from this role
    /// and provider model availability.
    pub(crate) selected_role: String,
    /// Model currently resolved from [`Self::selected_role`] and provider
    /// availability. `None` means the role has no provider-published model yet.
    pub(crate) selected_model: Option<ModelId>,
    /// State that belongs to exactly the currently bound session.
    /// Keep session-scoped counters here instead of as top-level
    /// harness fields, so `/session new` resets them with one assignment.
    pub(crate) current_session_state: CurrentSessionState,
    /// Provider/model for each prompt sent to the provider, used to
    /// attribute the corresponding finished response even if the user
    /// switches models while it is in flight.
    pub(crate) prompt_models: std::collections::HashMap<SessionPromptId, ModelId>,
    /// Per-prompt fingerprint of `(system_prompt, tools, model_params)`
    /// as observed on the outbound request. Read at response time to
    /// stamp `ChainAnchor::request_fingerprint`, so the anchor records
    /// what was *actually sent* even if the user flipped a setting
    /// between send and receive. See
    /// [`crate::conversation::compute_chain_fingerprint`].
    pub(crate) prompt_fingerprints:
        std::collections::HashMap<SessionPromptId, crate::conversation::ChainFingerprintDetail>,
    /// Skills discovered by extensions, keyed by name.
    pub(crate) discovered_skills: std::collections::HashMap<tau_proto::SkillName, DiscoveredSkill>,
    /// AGENTS.md files discovered by extensions, in delivery order.
    pub(crate) discovered_agents_files: Vec<DiscoveredAgentsFile>,
    /// Session-scoped JSON context contributions published by extensions.
    pub(crate) session_context: SessionContextStore,
    /// Extension-level prompt fragments keyed by source connection and name.
    pub(crate) extension_prompt_fragments:
        BTreeMap<tau_proto::ConnectionId, BTreeMap<String, PromptFragment>>,
    /// Loaded system prompt templates keyed by template name.
    pub(crate) system_prompt_templates: HashMap<String, String>,
    /// Sessions whose AGENTS/skill discovery has completed.
    pub(crate) initialized_sessions: std::collections::HashSet<SessionId>,
    /// Resumed sessions that still need a one-shot internal restore notice
    /// folded immediately before the next real user prompt, with the last
    /// durable event timestamp seen before resume when available.
    pub(crate) pending_restore_notice_sessions: HashMap<SessionId, Option<tau_proto::UnixMicros>>,
    /// Per-background-tool restore notes that should be folded immediately
    /// before the next real user prompt, not dispatched as standalone turns.
    pub(crate) pending_restore_background_notices: HashMap<SessionId, Vec<String>>,
    /// Tool availability notices waiting to be folded before the next real
    /// user prompt on the default conversation, keyed by internal tool name for
    /// deterministic delivery.
    pending_tool_availability_notices: BTreeMap<String, PendingToolAvailabilityNotice>,
    /// Tools whose unavailable notice has already been delivered and that are
    /// still absent from the registry. A later registration uses this to queue
    /// the matching available-again notice.
    unavailable_tool_notices_delivered: BTreeMap<String, ToolName>,
    /// Session prompt IDs that have already been completed by the agent.
    /// Used to dedupe duplicate `ProviderResponseFinished` events that can
    /// arise under at-least-once delivery (e.g. an agent that reconnects
    /// after a crash and replays its last prompt).
    pub(crate) completed_prompts: std::collections::HashSet<SessionPromptId>,
    /// Pure scheduler state for queued and in-flight tool invocations.
    pub(crate) tool_turn: ToolTurnMachine,
    /// Backgrounded calls whose real completion should not enqueue an internal
    /// model-visible steering prompt. The real result/error event is still
    /// published normally.
    pub(crate) suppressed_background_completion_prompts: HashSet<ToolCallId>,
    /// Owning conversations for background calls that have delivered their real
    /// completion. Kept so suppression can remove and later restore queued
    /// completion prompts across repeated wait/interrupt cycles.
    pub(crate) background_completion_targets: HashMap<ToolCallId, ConversationId>,
    /// Prompt ids canceled by `/cancel`. Late agent events for these
    /// prompts are ignored and never folded into session state.
    pub(crate) canceled_prompts: std::collections::HashSet<SessionPromptId>,
    /// In-flight auto-compaction summaries keyed by the temporary
    /// side-conversation that is generating them.
    pending_compactions: std::collections::HashMap<ConversationId, PendingCompaction>,
    /// Extension-started side-agent conversations waiting for the harness-owned
    /// global execution-mode scheduler. This queue is independent from normal
    /// per-conversation tool scheduling and applies to every
    /// `StartAgentRequest`, whether it came from delegate, notifications,
    /// or a future extension.
    pending_start_agent_requests: VecDeque<PendingStartAgentRequest>,
    /// Active extension-started side-agent conversations participating in the
    /// global sub-agent scheduler.
    active_start_agent_requests: std::collections::HashMap<ConversationId, ActiveStartAgentRequest>,
    /// State for harness-owned delegate/wait tools.
    pub(crate) subagents: SubagentToolState,
    /// Directory layout (config + state) the harness reads and writes.
    pub(crate) dirs: tau_config::settings::TauDirs,
}

#[cfg(any(test, feature = "echo-agent"))]
pub(crate) type ProviderRunner = fn(UnixStream, UnixStream) -> Result<(), String>;

/// One in-process tool extension to spawn alongside the echo provider during
/// tests.
#[cfg(any(test, feature = "echo-agent"))]
pub(crate) struct InProcessTool {
    pub(crate) name: &'static str,
    pub(crate) runner: fn(UnixStream, UnixStream) -> Result<(), String>,
}

/// A small echo provider used only by tests and echo-provider helpers.
#[cfg(any(test, feature = "echo-agent"))]
pub(crate) fn run_echo_provider<R, W>(
    reader: R,
    writer: W,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: std::io::Read,
    W: std::io::Write,
{
    use std::io::{BufReader, BufWriter};

    use tau_proto::{
        Ack, CborValue, ContentPart, ContextItem, ContextRole, Effort, EventName, FrameReader,
        FrameWriter, Hello, MessageItem, OpaqueProviderItem, PROTOCOL_VERSION, ProviderModelInfo,
        ProviderModelsUpdated, ProviderPromptSubmitted, Ready, Subscribe, ThinkingSummary,
        ToolCallItem, ToolName, Verbosity,
    };

    fn materialize_prompt(
        prompt: &tau_proto::SessionPromptCreated,
    ) -> tau_proto::SessionPromptCreated {
        let mut materialized = prompt.clone();
        materialized.tools_ref = None;
        materialized
    }

    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    writer.write_frame(&Frame::Message(Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "tau-echo-provider".into(),
        client_kind: ClientKind::Provider,
    })))?;
    // Live-only test provider: prompt, compaction, and cancel events are work
    // requests. Replaying past ones would rerun or cancel completed turns.
    writer.write_frame(&Frame::Message(Message::Subscribe(Subscribe {
        selectors: vec![
            EventSelector::Exact(EventName::SESSION_COMPACTION_REQUESTED),
            EventSelector::Exact(EventName::SESSION_PROMPT_CREATED),
            EventSelector::Exact(EventName::UI_CANCEL_PROMPT),
        ],
    })))?;
    writer.write_frame(&Frame::Event(Event::ProviderModelsUpdated(
        ProviderModelsUpdated {
            models: vec![ProviderModelInfo {
                id: "echo/model".into(),
                display_name: Some("Echo".to_owned()),
                default_affinity: 0,
                context_window: 128_000,
                efforts: vec![Effort::Off],
                verbosities: vec![Verbosity::Low],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: true,
            }],
        },
    )))?;
    writer.write_frame(&Frame::Message(Message::Ready(Ready {
        message: Some("echo provider ready".to_owned()),
    })))?;
    writer.flush()?;

    let mut next_call = 1_u64;

    loop {
        let Some(frame) = reader.read_frame()? else {
            return Ok(());
        };
        let (log_id, inner) = frame.peel_log();
        match inner {
            Frame::Event(Event::SessionCompactionRequested(request)) => {
                let spid = request.prompt.session_prompt_id.clone();
                let prompt = materialize_prompt(&request.prompt);
                writer.write_frame(&Frame::Event(Event::ProviderPromptSubmitted(
                    ProviderPromptSubmitted {
                        session_prompt_id: spid.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;
                writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(
                    ProviderResponseFinished {
                        session_prompt_id: spid,
                        target_agent_id: None,
                        output_items: vec![ContextItem::Compaction(OpaqueProviderItem(
                            CborValue::Map(vec![
                                (
                                    CborValue::Text("type".to_owned()),
                                    CborValue::Text("message".to_owned()),
                                ),
                                (
                                    CborValue::Text("role".to_owned()),
                                    CborValue::Text("assistant".to_owned()),
                                ),
                                (
                                    CborValue::Text("text".to_owned()),
                                    CborValue::Text("Conversation compacted.".to_owned()),
                                ),
                            ]),
                        ))],
                        stop_reason: ProviderStopReason::Compaction,
                        originator: prompt.originator.clone(),
                        usage: None,
                        backend: None,
                        provider_response_id: None,
                        ws_pool_delta: None,
                    },
                )))?;
                writer.flush()?;
            }
            Frame::Event(Event::SessionPromptCreated(prompt)) => {
                let spid = prompt.session_prompt_id.clone();
                let prompt = materialize_prompt(&prompt);
                writer.write_frame(&Frame::Event(Event::ProviderPromptSubmitted(
                    ProviderPromptSubmitted {
                        session_prompt_id: spid.clone(),
                        originator: prompt.originator.clone(),
                    },
                )))?;

                let is_tool_result = prompt
                    .context_items
                    .last()
                    .is_some_and(|item| matches!(item, ContextItem::ToolResult(_)));
                if is_tool_result {
                    let text = prompt
                        .context_items
                        .last()
                        .and_then(|item| match item {
                            ContextItem::ToolResult(result) => Some(result.output.render()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(
                        ProviderResponseFinished {
                            session_prompt_id: spid,
                            target_agent_id: None,
                            output_items: vec![ContextItem::Message(MessageItem {
                                role: ContextRole::Assistant,
                                content: vec![ContentPart::Text { text }],
                                phase: None,
                            })],
                            stop_reason: ProviderStopReason::EndTurn,
                            originator: prompt.originator.clone(),
                            usage: None,
                            backend: None,
                            provider_response_id: None,
                            ws_pool_delta: None,
                        },
                    )))?;
                } else {
                    let user_text = prompt
                        .context_items
                        .iter()
                        .rev()
                        .find_map(|item| match item {
                            ContextItem::Message(message) if message.role == ContextRole::User => {
                                message.content.first().map(|part| match part {
                                    ContentPart::Text { text } => text.clone(),
                                })
                            }
                            _ => None,
                        })
                        .unwrap_or_default();

                    let call_id = format!("call-{next_call}");
                    next_call += 1;

                    let tool_call = if let Some(path) = user_text.strip_prefix("read ") {
                        ToolCallItem {
                            call_id: call_id.into(),
                            name: ToolName::new("read"),
                            tool_type: tau_proto::ToolType::Function,
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("path".to_owned()),
                                CborValue::Text(path.trim().to_owned()),
                            )]),
                        }
                    } else if let Some(cmd) = user_text.strip_prefix("shell ") {
                        ToolCallItem {
                            call_id: call_id.into(),
                            name: ToolName::new("shell"),
                            tool_type: tau_proto::ToolType::Function,
                            arguments: CborValue::Map(vec![(
                                CborValue::Text("command".to_owned()),
                                CborValue::Text(cmd.trim().to_owned()),
                            )]),
                        }
                    } else {
                        ToolCallItem {
                            call_id: call_id.into(),
                            name: ToolName::new("echo"),
                            tool_type: tau_proto::ToolType::Function,
                            arguments: CborValue::Text(user_text),
                        }
                    };

                    writer.write_frame(&Frame::Event(Event::ProviderResponseFinished(
                        ProviderResponseFinished {
                            session_prompt_id: spid,
                            target_agent_id: None,
                            output_items: vec![ContextItem::ToolCall(tool_call)],
                            stop_reason: ProviderStopReason::ToolCalls,
                            originator: prompt.originator.clone(),
                            usage: None,
                            backend: None,
                            provider_response_id: None,
                            ws_pool_delta: None,
                        },
                    )))?;
                }
                writer.flush()?;
            }
            Frame::Message(Message::Disconnect(_)) => return Ok(()),
            _ => {}
        }
        if let Some(id) = log_id {
            writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
            writer.flush()?;
        }
    }
}

/// Returns a closure that mints monotonic `ExtensionInstanceId`s starting
/// at zero. Used during harness construction so each extension entry gets
/// a distinct id without a manually managed counter that's easy to leave
/// dangling when extensions are added or removed.
fn instance_id_factory() -> impl FnMut() -> tau_proto::ExtensionInstanceId {
    let mut counter: u64 = 0;
    move || {
        let iid = tau_proto::ExtensionInstanceId::new(counter);
        counter += 1;
        iid
    }
}

enum BackgroundCompletionPromptMode {
    QueueAndAdvance,
    QueueOnly,
}

impl Harness {
    #[cfg(any(test, feature = "echo-agent"))]
    pub(crate) fn new_with_provider(
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        provider_runner: ProviderRunner,
        tools: Vec<InProcessTool>,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
    ) -> Result<Self, HarnessError> {
        let state_dir = state_dir.into();
        let sessions_dir = tau_config::settings::sessions_dir_of(&state_dir);
        let (tx, rx) = mpsc::channel();
        let bus =
            EventBus::with_subscription_policy(Box::new(DefaultSubscriptionPolicy::with_store(
                PolicyStore::open(policy_store_path_from(&state_dir))?,
            )));
        // Lazy: only the eager session's tree is needed up front
        // (loaded below via `store.load_session`); other sessions
        // load on first access. Avoids a startup walk over every
        // historical session dir.
        let store = SessionStore::open_lazy(&sessions_dir)?;

        let own_pid = std::process::id();
        let mut next_iid = instance_id_factory();

        let mut extension_connects = Vec::new();
        // Provider
        let provider_spawn =
            spawn_in_process("provider", ClientKind::Provider, provider_runner, &tx)?;
        let provider_conn_id = provider_spawn.connection_id.clone();
        extension_connects.push(ExtensionConnectCommand {
            entry: ExtensionEntry {
                name: "provider".to_owned(),
                instance_id: next_iid(),
                connection_id: provider_conn_id,
                kind: ClientKind::Provider,
                pid: Some(own_pid),
                in_process_thread: Some(provider_spawn.thread),
                supervised_config: None,
                secrets: BTreeMap::new(),
                restart_attempt: 0,
                state: ExtensionState::Spawning,
                last_acked: tau_proto::LogEventId::default(),
            },
            origin: ConnectionOrigin::Supervised,
            writer_tx: provider_spawn.writer_tx,
            initialized_ack: provider_spawn.initialized_ack,
            replaces: None,
        });

        // Caller-supplied in-process tools.
        for tool in tools {
            let tool_spawn = spawn_in_process(tool.name, ClientKind::Tool, tool.runner, &tx)?;
            let conn_id = tool_spawn.connection_id.clone();
            extension_connects.push(ExtensionConnectCommand {
                entry: ExtensionEntry {
                    name: tool.name.to_owned(),
                    instance_id: next_iid(),
                    connection_id: conn_id,
                    kind: ClientKind::Tool,
                    pid: Some(own_pid),
                    in_process_thread: Some(tool_spawn.thread),
                    supervised_config: None,
                    secrets: BTreeMap::new(),
                    restart_attempt: 0,
                    state: ExtensionState::Spawning,
                    last_acked: tau_proto::LogEventId::default(),
                },
                origin: ConnectionOrigin::Supervised,
                writer_tx: tool_spawn.writer_tx,
                initialized_ack: tool_spawn.initialized_ack,
                replaces: None,
            });
        }

        let (harness_settings, harness_settings_error) = load_harness_settings_or_warn(&dirs);
        let system_prompt_templates = load_system_prompt_templates(dirs.config_dir.as_deref());
        let available_models = Vec::new();
        let LoadedRoles {
            roles: available_roles,
            role_overrides,
            selected_role,
            role_groups: available_role_groups,
            missing_default_role,
        } = load_roles(&harness_settings);
        if available_roles.is_empty() {
            return Err(HarnessError::Participant(
                "no roles are enabled; enable at least one role in harness.yaml or with --enable-role <role>".to_owned(),
            ));
        }
        let selected_model =
            select_model_for_role(&HashMap::new(), &available_roles, &selected_role);
        crate::session_cleanup::spawn_session_cleanup(
            sessions_dir.clone(),
            harness_settings.session_retention(),
        );
        let default_conversation_id = ConversationId::new("default");
        let mut store = store;
        let default_head = store
            .load_session(eager_session_id)?
            .and_then(|tree| tree.head());
        let mut conversations = std::collections::HashMap::new();
        conversations.insert(
            default_conversation_id.clone(),
            Conversation::new(
                default_conversation_id.clone(),
                eager_session_id.into(),
                tau_proto::PromptOriginator::User,
                default_head,
                None,
            ),
        );

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            action_registry: ActionRegistry::new(),
            internal_tool_handlers: Vec::new(),
            state_dir: state_dir.clone(),
            store,
            current_session_id: eager_session_id.into(),
            tool_conversations: std::collections::HashMap::new(),
            pending_tools: std::collections::HashMap::new(),
            completed_tool_calls: std::collections::HashSet::new(),
            pending_tool_providers: std::collections::HashMap::new(),
            pending_action_invocations: HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            extensions: std::collections::HashMap::new(),
            extension_activation_staging: std::collections::HashMap::new(),
            extension_order: Vec::new(),
            pending_extension_connects: 0,
            next_session_prompt_id: 0,
            prompt_conversations: std::collections::HashMap::new(),
            prompt_snapshots: std::collections::HashMap::new(),
            prompt_cache_diagnostics: std::collections::HashMap::new(),
            conversations,
            agent_conversations: HashMap::new(),
            agent_states: HashMap::new(),
            stopped_agent_ids: HashSet::new(),
            default_conversation_id,
            turn_state: TurnState::Idle,
            debug_log: None,
            interceptors: InterceptorRegistry::default(),
            pending_intercept: None,
            deferred_publishes: VecDeque::new(),
            pending_user_prompt_dispatches: VecDeque::new(),
            pending_publish_idle_dispatches: VecDeque::new(),
            available_models,
            provider_models_by_extension: HashMap::new(),
            provider_model_info: HashMap::new(),
            provider_model_routes: HashMap::new(),
            pending_provider_prompts: HashMap::new(),
            available_roles,
            available_role_groups,
            role_overrides,
            selected_role,
            selected_model,
            current_session_state: CurrentSessionState::default(),
            prompt_models: std::collections::HashMap::new(),
            prompt_fingerprints: std::collections::HashMap::new(),
            discovered_skills: built_in_discovered_skills(),
            discovered_agents_files: Vec::new(),
            session_context: SessionContextStore::default(),
            extension_prompt_fragments: BTreeMap::new(),
            system_prompt_templates,
            initialized_sessions: std::collections::HashSet::new(),
            pending_restore_notice_sessions: HashMap::new(),
            pending_restore_background_notices: HashMap::new(),
            pending_tool_availability_notices: BTreeMap::new(),
            unavailable_tool_notices_delivered: BTreeMap::new(),
            completed_prompts: std::collections::HashSet::new(),
            tool_turn: ToolTurnMachine::default(),
            suppressed_background_completion_prompts: HashSet::new(),
            background_completion_targets: HashMap::new(),
            canceled_prompts: std::collections::HashSet::new(),
            pending_compactions: std::collections::HashMap::new(),
            pending_start_agent_requests: VecDeque::new(),
            active_start_agent_requests: std::collections::HashMap::new(),
            subagents: SubagentToolState::default(),
            dirs,
        };

        // Debug log lives next to the eager-init session's events file
        // so the session dir stays self-contained: `events.cbor` +
        // `events.jsonl` + `meta.json` + `lock`.
        let _ = harness.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        // Record cwd in meta.json so `-r` (resume most recent for this
        // cwd) can find this session even before it has any log entries.
        // Also acquires the flock on `<sessions_dir>/<eager_session_id>/lock`.
        harness
            .store
            .record_session_meta(eager_session_id, std::env::current_dir().ok())?;

        harness.rehydrate_default_agent_from_session();
        harness.rehydrate_agent_states_from_session();

        for command in extension_connects {
            harness.queue_extension_connect(command)?;
        }
        harness.wait_for_extensions_ready()?;
        #[cfg(test)]
        harness.register_harness_tools();
        harness.publish_delegate_roles_context();
        harness.check_config_exists();
        harness.emit_startup_settings_errors(harness_settings_error);
        harness.emit_missing_default_role(missing_default_role);

        // Eager session init for the default session. INTENTIONAL —
        // do NOT "simplify" this to lazy-on-first-prompt.
        //
        // Reasons this is a design choice, not dead weight:
        //
        // 1. **Pre-warm AGENTS.md and skill discovery.** The default session is the
        //    fallback when a caller (embedded or socket) doesn't specify one, and even
        //    when callers pick their own `chat-<ts>` id they still benefit: ext-shell
        //    has already walked `~/.agents/` + the cwd ancestor chain once, so the
        //    second init is cache-warm.
        //
        // 2. **Surface discovery before the first prompt.** The CLI prints "loaded: …"
        //    as events arrive; doing this at startup gives the user visible
        //    confirmation that their AGENTS.md was found — before they type anything —
        //    instead of bundling that feedback into the first agent response.
        //
        // 3. **Fail loudly at startup, not mid-first-turn.** If a provider hangs or the
        //    discovery logic panics, the process hits `StartupTimeout` here rather than
        //    appearing to accept the first prompt and then silently stalling.
        //
        // Every past agent that touched this code has "noticed" that
        // the CLI uses `chat-<ts>` session ids and concluded the eager
        // init is wasted work. It isn't. Please resist the urge.
        harness.start_session_init(eager_session_id.into(), eager_session_start_reason);
        harness.wait_for_session_init()?;
        Ok(harness)
    }

    /// Creates a harness from configuration, spawning real child processes.
    pub(crate) fn from_config(
        config: &Config,
        state_dir: impl Into<PathBuf>,
        dirs: tau_config::settings::TauDirs,
        eager_session_id: &str,
        eager_session_start_reason: tau_proto::SessionStartReason,
    ) -> Result<Self, HarnessError> {
        let startup_started_at = Instant::now();
        tracing::debug!(target: "tau_harness::startup", eager_session_id, "constructing harness from config");
        let state_dir = state_dir.into();
        let sessions_dir = tau_config::settings::sessions_dir_of(&state_dir);
        let (tx, rx) = mpsc::channel();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "opening policy store");
        let policy_store = PolicyStore::open(policy_store_path_from(&state_dir))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "policy store opened");
        let bus = EventBus::with_subscription_policy(Box::new(
            DefaultSubscriptionPolicy::with_store(policy_store),
        ));
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "opening session store");
        let store = SessionStore::open_lazy(&sessions_dir)?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session store opened");

        let secret_sources =
            load_secret_sources().map_err(|error| HarnessError::Participant(error.to_string()))?;
        let extension_secrets = resolve_extension_secrets(config, &state_dir, &secret_sources)
            .map_err(|error| HarnessError::Participant(error.to_string()))?;

        let mut extension_connects = Vec::new();
        let mut next_iid = instance_id_factory();

        for ext_config in config.extensions.values() {
            tracing::info!(
                target: "tau_harness::startup",
                extension = %ext_config.name,
                command = %ext_config.command,
                args = ?ext_config.args,
                elapsed_ms = startup_started_at.elapsed().as_millis(),
                "spawning extension",
            );
            let kind = match ext_config.role.as_deref() {
                Some("provider") => ClientKind::Provider,
                _ => ClientKind::Tool,
            };

            let log_path =
                extension_stderr_log_path(&sessions_dir, eager_session_id, &ext_config.name)
                    .map_err(|error| HarnessError::Participant(error.to_string()))?;
            let spawned = spawn_supervised(ext_config, kind.clone(), Some(log_path), &tx)?;
            let conn_id = spawned.connection_id.clone();
            tracing::info!(
                target: "tau_harness::startup",
                extension = %ext_config.name,
                pid = spawned.child_pid,
                elapsed_ms = startup_started_at.elapsed().as_millis(),
                "extension spawned",
            );

            extension_connects.push(ExtensionConnectCommand {
                entry: ExtensionEntry {
                    name: ext_config.name.clone(),
                    instance_id: next_iid(),
                    connection_id: conn_id,
                    kind: kind.clone(),
                    pid: Some(spawned.child_pid),
                    in_process_thread: None,
                    supervised_config: Some(ext_config.clone()),
                    secrets: extension_secrets
                        .get(&ext_config.name)
                        .cloned()
                        .unwrap_or_default(),
                    restart_attempt: 0,
                    state: ExtensionState::Spawning,
                    last_acked: tau_proto::LogEventId::default(),
                },
                origin: ConnectionOrigin::Supervised,
                writer_tx: spawned.writer_tx,
                initialized_ack: spawned.initialized_ack,
                replaces: None,
            });
        }

        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "loading harness settings");
        let (harness_settings, harness_settings_error) = load_harness_settings_or_warn(&dirs);
        let system_prompt_templates = load_system_prompt_templates(dirs.config_dir.as_deref());
        let available_models = Vec::new();
        let LoadedRoles {
            roles: available_roles,
            role_overrides,
            selected_role,
            role_groups: available_role_groups,
            missing_default_role,
        } = load_roles(&harness_settings);
        if available_roles.is_empty() {
            return Err(HarnessError::Participant(
                "no roles are enabled; enable at least one role in harness.yaml or with --enable-role <role>".to_owned(),
            ));
        }
        let selected_model =
            select_model_for_role(&HashMap::new(), &available_roles, &selected_role);
        tracing::debug!(target: "tau_harness::startup", selected_model = ?selected_model, elapsed_ms = startup_started_at.elapsed().as_millis(), "harness settings loaded");
        crate::session_cleanup::spawn_session_cleanup(
            sessions_dir.clone(),
            harness_settings.session_retention(),
        );
        let default_conversation_id = ConversationId::new("default");
        let mut store = store;
        let default_head = store
            .load_session(eager_session_id)?
            .and_then(|tree| tree.head());
        let mut conversations = std::collections::HashMap::new();
        conversations.insert(
            default_conversation_id.clone(),
            Conversation::new(
                default_conversation_id.clone(),
                eager_session_id.into(),
                tau_proto::PromptOriginator::User,
                default_head,
                None,
            ),
        );

        let mut harness = Self {
            tx,
            rx,
            bus,
            registry: ToolRegistry::new(),
            action_registry: ActionRegistry::new(),
            internal_tool_handlers: Vec::new(),
            state_dir: state_dir.clone(),
            store,
            current_session_id: eager_session_id.into(),
            tool_conversations: std::collections::HashMap::new(),
            pending_tools: std::collections::HashMap::new(),
            completed_tool_calls: std::collections::HashSet::new(),
            pending_tool_providers: std::collections::HashMap::new(),
            pending_action_invocations: HashMap::new(),
            event_log: EventLog::new(),
            client_writers: std::collections::HashMap::new(),
            lifecycle_messages: Vec::new(),
            extensions: std::collections::HashMap::new(),
            extension_activation_staging: std::collections::HashMap::new(),
            extension_order: Vec::new(),
            pending_extension_connects: 0,
            next_session_prompt_id: 0,
            prompt_conversations: std::collections::HashMap::new(),
            prompt_snapshots: std::collections::HashMap::new(),
            prompt_cache_diagnostics: std::collections::HashMap::new(),
            conversations,
            agent_conversations: HashMap::new(),
            agent_states: HashMap::new(),
            stopped_agent_ids: HashSet::new(),
            default_conversation_id,
            turn_state: TurnState::Idle,
            debug_log: None,
            interceptors: InterceptorRegistry::default(),
            pending_intercept: None,
            deferred_publishes: VecDeque::new(),
            pending_user_prompt_dispatches: VecDeque::new(),
            pending_publish_idle_dispatches: VecDeque::new(),
            available_models,
            provider_models_by_extension: HashMap::new(),
            provider_model_info: HashMap::new(),
            provider_model_routes: HashMap::new(),
            pending_provider_prompts: HashMap::new(),
            available_roles,
            available_role_groups,
            role_overrides,
            selected_role,
            selected_model,
            current_session_state: CurrentSessionState::default(),
            prompt_models: std::collections::HashMap::new(),
            prompt_fingerprints: std::collections::HashMap::new(),
            discovered_skills: built_in_discovered_skills(),
            discovered_agents_files: Vec::new(),
            session_context: SessionContextStore::default(),
            extension_prompt_fragments: BTreeMap::new(),
            system_prompt_templates,
            initialized_sessions: std::collections::HashSet::new(),
            pending_restore_notice_sessions: HashMap::new(),
            pending_restore_background_notices: HashMap::new(),
            pending_tool_availability_notices: BTreeMap::new(),
            unavailable_tool_notices_delivered: BTreeMap::new(),
            completed_prompts: std::collections::HashSet::new(),
            tool_turn: ToolTurnMachine::default(),
            suppressed_background_completion_prompts: HashSet::new(),
            background_completion_targets: HashMap::new(),
            canceled_prompts: std::collections::HashSet::new(),
            pending_compactions: std::collections::HashMap::new(),
            pending_start_agent_requests: VecDeque::new(),
            active_start_agent_requests: std::collections::HashMap::new(),
            subagents: SubagentToolState::default(),
            dirs,
        };

        let _ = harness.enable_debug_log(&sessions_dir.join(eager_session_id))?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "debug event log enabled");
        // Record cwd in meta.json so `-r` (resume most recent for this
        // cwd) can find this session even before it has any log entries.
        // Also acquires the flock on `<sessions_dir>/<eager_session_id>/lock`.
        harness
            .store
            .record_session_meta(eager_session_id, std::env::current_dir().ok())?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session metadata recorded");

        harness.rehydrate_default_agent_from_session();
        harness.rehydrate_agent_states_from_session();

        for command in extension_connects {
            harness.queue_extension_connect(command)?;
        }
        harness.wait_for_extensions_ready()?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "extensions ready");
        #[cfg(test)]
        harness.register_harness_tools();
        harness.publish_delegate_roles_context();
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "harness tools registered");
        harness.check_config_exists();
        harness.emit_startup_settings_errors(harness_settings_error);
        harness.emit_missing_default_role(missing_default_role);
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "config checks complete");

        harness.start_session_init(eager_session_id.into(), eager_session_start_reason);
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session init started");
        harness.wait_for_session_init()?;
        tracing::debug!(target: "tau_harness::startup", elapsed_ms = startup_started_at.elapsed().as_millis(), "session init complete");
        Ok(harness)
    }

    fn log_event(&mut self, harness_event: &HarnessEvent) {
        if let Some(log) = &mut self.debug_log {
            log.log_harness_event(harness_event);
        }
    }

    fn queue_extension_connect(
        &mut self,
        command: ExtensionConnectCommand,
    ) -> Result<(), HarnessError> {
        self.pending_extension_connects += 1;
        if self
            .tx
            .send(HarnessEvent::Command(HarnessCommand::ConnectExtension(
                Box::new(command),
            )))
            .is_ok()
        {
            return Ok(());
        }
        self.pending_extension_connects -= 1;
        Err(HarnessError::Participant(
            "harness command channel closed".to_owned(),
        ))
    }

    fn handle_harness_command(&mut self, command: HarnessCommand) -> Result<(), HarnessError> {
        match command {
            HarnessCommand::ConnectExtension(command) => self.connect_extension(*command),
        }
        Ok(())
    }

    fn connect_extension(&mut self, command: ExtensionConnectCommand) {
        let ExtensionConnectCommand {
            entry,
            origin,
            writer_tx,
            initialized_ack,
            replaces,
        } = command;
        let connection_id = entry.connection_id.clone();
        let name = entry.name.clone();
        let kind = entry.kind.clone();

        let connected_id = self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: connection_id.clone(),
                name: name.clone(),
                kind,
                origin,
            },
            Box::new(ChannelSink { tx: writer_tx }),
        ));
        debug_assert_eq!(connected_id, connection_id);

        if let Some(replaced) = replaces {
            self.extensions.remove(&replaced);
            self.extension_activation_staging.remove(&replaced);
            if let Some(slot) = self.extension_order.iter_mut().find(|id| **id == replaced) {
                *slot = connection_id.clone();
            } else if !self.extension_order.iter().any(|id| id == &connection_id) {
                self.extension_order.push(connection_id.clone());
            }
        } else if !self.extension_order.iter().any(|id| id == &connection_id) {
            self.extension_order.push(connection_id.clone());
        }
        self.extension_activation_staging
            .insert(connection_id.clone(), ExtensionActivationStage::default());
        self.extensions.insert(connection_id, entry);
        if 0 < self.pending_extension_connects {
            self.pending_extension_connects -= 1;
        }
        self.emit_extension_starting(&name);
        let _ = initialized_ack.send(());
    }

    /// Session id of the conversation that owns a given in-flight
    /// prompt, or `None` if the prompt id is unknown.
    fn session_id_for_prompt(&self, spid: &SessionPromptId) -> Option<SessionId> {
        let cid = self.prompt_conversations.get(spid)?;
        self.conversations.get(cid).map(|c| c.session_id.clone())
    }

    /// Session id of the conversation that owns a given in-flight
    /// tool call, or `None` if the call id is unknown.
    fn session_id_for_tool_call(&self, call_id: &ToolCallId) -> Option<SessionId> {
        let cid = self.tool_conversations.get(call_id)?;
        self.conversations.get(cid).map(|c| c.session_id.clone())
    }

    /// Conversation id that owns a given in-flight prompt, if any.
    fn conversation_for_prompt(&self, spid: &SessionPromptId) -> Option<ConversationId> {
        self.prompt_conversations.get(spid).cloned()
    }

    /// If the conversation's dedup map's "built for" cursor doesn't
    /// match its current `head`, rebuild it from the assembled branch.
    /// O(branch_len) on rebuild; O(1) on the steady-state hot path
    /// where the linear-extension hook in [`Self::commit_event`] keeps
    /// `built_for` in sync after every fold.
    ///
    /// `None` is returned only if the conversation no longer exists
    /// (the caller raced its own teardown), and the caller treats that
    /// as "skip dedup, just publish".
    fn ensure_dedup_built_for_branch(&mut self, cid: &ConversationId) -> Option<()> {
        let head = self.conversations.get(cid)?.head;
        let needs = self
            .conversations
            .get(cid)
            .map(|c| c.result_dedup.needs_rebuild(head))
            .unwrap_or(false);
        if !needs {
            return Some(());
        }
        // Walk the branch under an immutable borrow of the store, then
        // hand the snapshot to the conversation under a mut borrow —
        // the branch iterator borrows the tree, so we materialize it
        // into an owned Vec first to release the tree borrow.
        let session_id = self.conversations.get(cid)?.session_id.clone();
        let branch: Vec<tau_core::SessionEntry> = self
            .store
            .session(session_id.as_str())
            .map(|t| t.branch_from(head).into_iter().cloned().collect())
            .unwrap_or_default();
        let conv = self.conversations.get_mut(cid)?;
        conv.result_dedup
            .rebuild_from_branch(branch.iter(), head, DEFAULT_THRESHOLD_BYTES);
        Some(())
    }

    /// Replace `result.result` with a pointer if a previous tool
    /// result on this conversation's branch has the same content.
    /// Mutates `result` in place; the caller publishes the (possibly
    /// modified) value, which is what gets folded into the tree and
    /// what the LLM sees on the next turn.
    fn dedup_tool_result(&mut self, cid: &ConversationId, result: &mut tau_proto::ToolResult) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_for_hash(&result.result);
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.conversations.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.result_dedup.lookup(&hash).cloned() {
            // Belt-and-suspenders: refuse to point a call at itself.
            // This can't happen in practice — `tool_conversations`
            // already drops the call_id between intake and now — but
            // a future change to the tracking map could let a tool
            // result re-enter this path twice, and self-pointing is a
            // worse failure mode than just skipping the dedup.
            if original_call_id == result.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %result.tool_name,
                call_id = %result.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool result against earlier identical output"
            );
            result.result = build_pointer_value(&original_call_id, &result.tool_name);
        } else {
            conv.result_dedup.insert(hash, result.call_id.clone());
        }
    }

    /// Companion to [`Self::dedup_tool_result`] for `ToolError`s.
    /// Same semantics — collapses repeated identical errors (same
    /// message, same `details`) into a pointer back to the first
    /// occurrence on this branch.
    fn dedup_tool_error(&mut self, cid: &ConversationId, error: &mut tau_proto::ToolError) {
        if self.ensure_dedup_built_for_branch(cid).is_none() {
            return;
        }
        let bytes = encode_error_for_hash(&error.message, error.details.as_ref());
        if bytes.len() < DEFAULT_THRESHOLD_BYTES {
            return;
        }
        let hash = hash_truncated(&bytes);
        let Some(conv) = self.conversations.get_mut(cid) else {
            return;
        };
        if let Some(original_call_id) = conv.result_dedup.lookup(&hash).cloned() {
            if original_call_id == error.call_id {
                return;
            }
            tracing::debug!(
                target: "tau_harness",
                cid = %cid,
                tool = %error.tool_name,
                call_id = %error.call_id,
                points_to = %original_call_id,
                bytes = bytes.len(),
                "deduping tool error against earlier identical output"
            );
            error.message = build_pointer_error_message(&original_call_id, &error.tool_name);
            error.details = None;
        } else {
            conv.result_dedup.insert(hash, error.call_id.clone());
        }
    }

    /// Publishes an event for a specific conversation. The fold uses
    /// the conversation's `head` as the explicit parent — no more
    /// `UiNavigateTree` head-bouncing — and the post-commit hook in
    /// [`Harness::commit_event`] keeps `c.head` in sync with the
    /// freshly-folded node.
    ///
    /// This helper is what makes branching prompts work: the default
    /// (user) conversation can keep advancing while a side conversation
    /// from an extension grows its own branch off some earlier node;
    /// each side publish brackets its own navigate-then-append.
    pub(crate) fn publish_for_conversation(&mut self, cid: &ConversationId, event: Event) {
        self.publish_for_conversation_from(cid, None, event);
    }

    fn publish_terminal_tool_result(
        &mut self,
        cid: Option<&ConversationId>,
        source: Option<&str>,
        result: ToolResult,
    ) {
        match cid {
            Some(cid) => {
                self.publish_for_conversation_from(cid, source, Event::ToolResult(result.clone()));
                self.publish_for_conversation_from(
                    cid,
                    source,
                    Event::ProviderToolResult(result.clone()),
                );
            }
            None => {
                self.publish_event(source, Event::ToolResult(result.clone()));
                self.publish_event(source, Event::ProviderToolResult(result.clone()));
            }
        }
        self.record_wait_tool_result(result);
    }

    fn publish_terminal_tool_error(
        &mut self,
        cid: Option<&ConversationId>,
        source: Option<&str>,
        error: ToolError,
    ) {
        match cid {
            Some(cid) => {
                self.publish_for_conversation_from(cid, source, Event::ToolError(error.clone()));
                self.publish_for_conversation_from(
                    cid,
                    source,
                    Event::ProviderToolError(error.clone()),
                );
            }
            None => {
                self.publish_event(source, Event::ToolError(error.clone()));
                self.publish_event(source, Event::ProviderToolError(error.clone()));
            }
        }
        self.record_wait_tool_error(error);
    }

    fn publish_terminal_background_error(
        &mut self,
        cid: &ConversationId,
        source: Option<&str>,
        error: ToolBackgroundError,
    ) {
        self.publish_for_conversation_from(cid, source, Event::ToolBackgroundError(error.clone()));
        self.record_wait_background_error(error);
    }

    /// Like [`publish_for_conversation`] but lets the caller record an
    /// originating connection on the persisted record (for `tool.result`
    /// / `tool.error` arriving from extensions). The snap-to-`cid`-head
    /// step is what keeps cross-conversation tool activity from folding
    /// onto the wrong tree branch — without it, a sibling side conv that
    /// just navigated `tree.head` would steal the parent of the next
    /// tree-folding event.
    fn publish_for_conversation_from(
        &mut self,
        cid: &ConversationId,
        source: Option<&str>,
        event: Event,
    ) {
        // Stamp the publish with `cid`. The fold reads the
        // conversation's `head` as the explicit parent node in
        // `commit_event`, so cross-conversation publishes no longer
        // need a `UiNavigateTree` round-trip to bounce the global
        // write cursor. After the commit, the post-commit hook
        // also syncs `c.head` automatically — the trailing
        // read-tree-and-update idiom is gone entirely.
        //
        // Re-stamp tool events with the owning conversation's
        // originator so subscribers can tell main-agent tool
        // activity from sub-agent tool activity without having to
        // map `call_id` back to a conversation themselves. Construction
        // sites can leave `originator` as the default — this is the
        // single point of truth.
        let event =
            if let Some(originator) = self.conversations.get(cid).map(|c| c.originator.clone()) {
                stamp_tool_event_originator(event, originator)
            } else {
                event
            };
        self.publish_event_for_conversation(cid, source, event);
    }

    /// Publishes an event to both the event bus and the event log.
    /// Convenience wrapper that uses the event's default transience
    /// and never marks the publish as `must_pass`.
    pub(crate) fn publish_event(&mut self, source: Option<&str>, event: Event) {
        let transient = event.defaults_to_transient();
        self.enqueue_publish(source, event, transient, false, None);
    }

    /// Like [`Harness::publish_event`] but tags the publish with the
    /// originating conversation. After the event commits, the
    /// harness syncs that conversation's cached `head` to the
    /// freshly-folded `tree.head()` — so callers don't need to read
    /// the tree themselves (which would race the interception chain
    /// when a publish parks).
    fn publish_event_for_conversation(
        &mut self,
        cid: &ConversationId,
        source: Option<&str>,
        event: Event,
    ) {
        if !self.conversations.contains_key(cid) {
            // The conversation was torn down between when the
            // caller looked it up and now (e.g. side conv that
            // raced its own teardown with a late tool result).
            // Fall back to a plain publish so the event still
            // reaches the bus / log; we just can't stamp a parent
            // for it.
            tracing::warn!(
                target: "tau_harness",
                event = %event.name(),
                cid = %cid,
                "publish_event_for_conversation called with unknown cid; \
                 publishing without parent stamp",
            );
            self.publish_event(source, event);
            return;
        }
        let transient = event.defaults_to_transient();
        let Some(session_id) = self.conversations.get(cid).map(|c| c.session_id.clone()) else {
            // The conversation was torn down between the existence
            // check above and now. Fall back to a plain publish so
            // the event still reaches subscribers; persistence will
            // use the event's own attribution if it still has one.
            self.enqueue_publish(source, event, transient, false, None);
            return;
        };
        let sync = Some(ConversationHeadSync {
            cid: cid.clone(),
            session_id,
        });
        self.enqueue_publish(source, event, transient, false, sync);
    }

    fn materialize_session_prompt_created(
        &self,
        prompt: &SessionPromptCreated,
    ) -> Option<SessionPromptCreated> {
        let mut materialized = prompt.clone();
        if let Some(tools_ref) = &prompt.tools_ref {
            let base = self
                .prompt_snapshots
                .get(&tools_ref.base_session_prompt_id)?;
            materialized.tools = base.tools.clone();
            materialized.tools_ref = None;
        }
        Some(materialized)
    }

    fn note_session_prompt_created(&mut self, prompt: &SessionPromptCreated) {
        let Some(materialized) = self.materialize_session_prompt_created(prompt) else {
            tracing::warn!(
                target: "tau_harness",
                session_prompt_id = %prompt.session_prompt_id,
                "could not materialize committed session.prompt_created"
            );
            return;
        };
        self.prompt_snapshots
            .insert(materialized.session_prompt_id.clone(), materialized.clone());
        if let Some(cid) = self
            .prompt_conversations
            .get(&materialized.session_prompt_id)
            .cloned()
            && let Some(conv) = self.conversations.get_mut(&cid)
        {
            conv.last_prompt_id = Some(materialized.session_prompt_id);
        }
    }

    fn provider_route_for_prompt_request(&self, event: &Event) -> Option<tau_proto::ConnectionId> {
        let model = match event {
            Event::SessionPromptCreated(prompt) => prompt.model.as_ref(),
            Event::SessionCompactionRequested(request) => request.prompt.model.as_ref(),
            _ => None,
        }?;
        self.provider_model_routes.get(model).cloned()
    }

    fn track_provider_prompt_request(
        &mut self,
        event: &Event,
        provider_connection_id: tau_proto::ConnectionId,
    ) {
        let Some(session_prompt_id) = (match event {
            Event::SessionPromptCreated(prompt) => Some(&prompt.session_prompt_id),
            Event::SessionCompactionRequested(request) => Some(&request.prompt.session_prompt_id),
            _ => None,
        }) else {
            return;
        };
        self.pending_provider_prompts
            .insert(session_prompt_id.clone(), provider_connection_id);
    }

    /// Final commit: persist (when applicable), append to the event
    /// log, and broadcast on the bus. Does not consult interception
    /// state — the caller is responsible for getting here only when
    /// the chain has resolved. Triggers any post-commit reactions
    /// (currently: deferred agent dispatches that were waiting on
    /// this user-message-bearing event to land in the tree, plus
    /// per-publish conversation `head` syncs).
    pub(crate) fn commit_event(
        &mut self,
        source: Option<&str>,
        event: Event,
        transient: bool,
        sync_head_for: Option<ConversationHeadSync>,
    ) {
        // When this publish was stamped with a conversation, fold
        // the event onto that conversation's branch directly. This
        // skips the `UiNavigateTree` head-bouncing dance that
        // `publish_for_conversation_from` used to do — the explicit
        // parent in `apply_event_at` does the same job without
        // touching the global cursor.
        let parent_for_fold = if sync_head_for.as_ref().is_some_and(|s| {
            self.conversations
                .get(&s.cid)
                .is_some_and(|c| c.head.is_none())
        }) {
            Some(None)
        } else {
            sync_head_for
                .as_ref()
                .and_then(|s| self.conversations.get(&s.cid).and_then(|c| c.head))
                .map(Some)
        };
        // Stamp once and share with every downstream observer: the
        // durable record on disk, the in-memory event log entry, and
        // the wire `LogEvent` envelope. Sampling the clock three
        // separate times would let timing analyses disagree with what
        // live subscribers saw.
        let source_id = source.map(tau_proto::ConnectionId::from);
        let (seq, recorded_at) = self.event_log.append(source_id.clone(), event.clone());
        // Mirror every committed event into the JSONL debug log as a
        // `published` line. The inbound `from_connection` lines carry
        // the raw frame the agent sent us, but for events that the
        // harness enriches (notably `ProviderResponseFinished`, where
        // `token_usage` is built here from session-wide state the
        // agent never sees), the enriched payload only exists on the
        // outbound copy. Offline cache/cost analysis tools that read
        // `events.jsonl` would otherwise see zeros where the running
        // session totals belong.
        if let Some(log) = &mut self.debug_log {
            log.log_published_event(source_id.as_ref(), &event, recorded_at);
        }
        let session_id = sync_head_for.as_ref().map(|s| s.session_id.clone());
        let folded_node_id = match self.persist_session_event(
            source,
            &event,
            transient,
            parent_for_fold,
            session_id.as_ref(),
            recorded_at,
        ) {
            Ok(folded_node_id) => folded_node_id,
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event.name(),
                    %error,
                    "dropping event rejected by session store"
                );
                self.emit_info(&format!(
                    "event {} rejected by session store: {error}",
                    event.name()
                ));
                return;
            }
        };
        if let Event::SessionPromptCreated(prompt) = &event {
            self.note_session_prompt_created(prompt);
        }
        if let Some(sync) = sync_head_for
            && let Some(node_id) = folded_node_id
            && let Some(c) = self.conversations.get_mut(&sync.cid)
        {
            // Only advance the conversation's own branch cursor when
            // the event produced a tree node. `tree.head()` is the
            // *global* write cursor and may sit on a sibling
            // conversation's last fold; syncing to it after a
            // non-folding event (e.g. `ProviderResponseFinished` with
            // only tool calls) would graft this conversation's next
            // tool tool request onto the wrong branch and produce orphan
            // ToolUse blocks downstream.
            c.head = Some(node_id);
            // Keep the dedup map's "built for" cursor in lockstep with
            // the just-folded linear extension. The dedup-decision
            // path already inserted any new (hash, call_id) entry
            // before the publish, so the map's contents already match
            // what a fresh rebuild from this new head would produce.
            // Bumping the cursor here lets the next tool result skip
            // the rebuild entirely (the steady-state hot path).
            //
            // We pass *every* fold through this hook, including ones
            // that didn't touch the dedup map (a `UserMessage` from
            // session re-init, an `AgentMessage`, a `ToolRequest`
            // node). [`ResultDedupMap::note_head_advanced_to`] guards
            // against the dangerous case — `built_for == None` plus a
            // non-dedup-eligible fold — by skipping the bump, so the
            // rebuild still triggers on the next dedup intake. Don't
            // gate this call on the event variant: that would re-couple
            // `commit_event` to per-tool semantics that the dedup
            // module deliberately owns.
            c.result_dedup.note_head_advanced_to(node_id);
        }
        // Wrap in a `LogEvent` message envelope so subscribers get the
        // id and can ack after processing. Receivers that don't care
        // (UIs) call `Frame::peel_log()` and discard the id.
        let log_frame = Frame::Message(Message::LogEvent(tau_proto::LogEvent {
            id: tau_proto::LogEventId::new(seq),
            recorded_at,
            event: Box::new(event.clone()),
        }));
        if let Some(provider_connection_id) = self.provider_route_for_prompt_request(&event) {
            // Provider-owned prompt execution is point-to-point: observers still
            // see the durable prompt fact, but execution clients do not all race
            // to consume it. The owning provider gets the exact same LogEvent
            // envelope via a directed route so ACK and replay semantics match
            // the subscribed-provider path.
            let execution_kinds = [ClientKind::Provider];
            let _ =
                self.bus
                    .publish_from_excluding_kinds(source, log_frame.clone(), &execution_kinds);
            match self
                .bus
                .send_to(provider_connection_id.as_str(), source, log_frame)
            {
                Ok(report) if !report.delivered_to.is_empty() => {
                    self.track_provider_prompt_request(&event, provider_connection_id);
                }
                Ok(report) => {
                    tracing::warn!(
                        target: "tau_harness",
                        event = %event.name(),
                        provider_connection_id = %provider_connection_id,
                        ?report,
                        "provider prompt route did not deliver"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "tau_harness",
                        event = %event.name(),
                        provider_connection_id = %provider_connection_id,
                        %error,
                        "provider prompt route failed"
                    );
                }
            }
        } else {
            let _ = self.bus.publish_from(source, log_frame);
        }
        if let Err(error) = self.dispatch_internal_tool_event(&event) {
            self.emit_info(&format!("internal tool event handler failed: {error}"));
        }
        self.react_to_committed_event(&event);
    }

    /// Post-commit reactions. Drains the deferred-agent-dispatch
    /// queue when a user-message-bearing event commits, so the
    /// agent prompt assembled in `send_prompt_to_agent_for` sees
    /// the just-folded user message. The `c.head` sync that this
    /// dispatch depends on is handled inside `commit_event` for any
    /// publish stamped via `publish_event_for_conversation`.
    fn react_to_committed_event(&mut self, event: &Event) {
        if let Event::AgentMessage(message) = event {
            self.deliver_agent_message(message);
        }
        let folds_user_message = matches!(
            event,
            Event::UiPromptSubmitted(_)
                | Event::SessionUserMessageInjected(_)
                | Event::SessionPromptSteered(_)
        );
        if !folds_user_message {
            return;
        }
        let Some(cid) = self.pending_user_prompt_dispatches.pop_front() else {
            return;
        };
        if !self.conversations.contains_key(&cid) {
            // Conversation was torn down while the prompt was in
            // limbo (e.g. side query that timed out).
            return;
        }
        self.send_prompt_to_agent_for(&cid);
    }

    /// Classify whether a non-user message recipient can receive a hidden
    /// prompt.
    pub(crate) fn agent_message_recipient_status(
        &self,
        recipient_id: &str,
    ) -> AgentMessageRecipientStatus {
        if self.agent_conversations.contains_key(recipient_id)
            || self
                .pending_start_agent_requests
                .iter()
                .any(|pending| pending.agent_id == recipient_id)
        {
            AgentMessageRecipientStatus::Live
        } else if self.stopped_agent_ids.contains(recipient_id) {
            AgentMessageRecipientStatus::Stopped
        } else {
            AgentMessageRecipientStatus::Unknown
        }
    }

    fn deliver_agent_message(&mut self, message: &tau_proto::AgentMessage) {
        if message.recipient_id == "user" {
            return;
        }
        let text = format!(
            "[tau-internal]: You have received a message from {}\n\n<message>\n{}\n</message>",
            message.sender_id,
            escape_agent_message_for_prompt(&message.message)
        );
        if let Some(cid) = self.agent_conversations.get(&message.recipient_id).cloned() {
            if let Some(conv) = self.conversations.get_mut(&cid) {
                conv.pending_prompts
                    .push_back(PendingPrompt::agent_message(text));
            }
            self.try_advance_queue();
            return;
        }
        if let Some(pending) = self
            .pending_start_agent_requests
            .iter_mut()
            .find(|pending| pending.agent_id == message.recipient_id)
        {
            pending
                .pending_agent_messages
                .push_back(PendingPrompt::agent_message(text));
        }
    }

    /// Persists `event` to the durable per-session log and folds it
    /// into the in-memory tree. Returns the id of the just-folded
    /// node when the event produced one (e.g. `UserMessage`,
    /// `ToolActivity`, an `AgentMessage` from a finished response
    /// with text), or `None` for transient / non-folding events. The
    /// caller uses the returned node id to sync a per-conversation
    /// branch cursor without consulting the tree's *global* head,
    /// which can be on a sibling conversation's branch.
    fn persist_session_event(
        &mut self,
        source: Option<&str>,
        event: &Event,
        transient: bool,
        parent_node_id: Option<Option<tau_proto::NodeId>>,
        session_id_override: Option<&SessionId>,
        recorded_at: tau_proto::UnixMicros,
    ) -> Result<Option<tau_proto::NodeId>, SessionStoreError> {
        if transient
            && !matches!(
                event,
                Event::ToolResult(_)
                    | Event::ToolError(_)
                    | Event::ProviderToolResult(_)
                    | Event::ProviderToolError(_)
                    | Event::ToolCancelled(_)
            )
        {
            return Ok(None);
        }
        let Some(session_id) = session_id_override
            .cloned()
            .or_else(|| self.session_id_for_event(event))
        else {
            return Ok(None);
        };
        let source = source.map(tau_proto::ConnectionId::from);
        Ok(self
            .store
            .append_session_event_at(
                session_id.as_str(),
                source,
                parent_node_id,
                event.clone(),
                recorded_at,
            )?
            .folded_node_id)
    }

    fn session_id_for_event(&self, event: &Event) -> Option<SessionId> {
        self.ui_session_id_for_event(event)
            .or_else(|| self.session_event_session_id(event))
            .or_else(|| self.provider_event_session_id(event))
            .or_else(|| self.tool_event_session_id(event))
            .or_else(|| self.extension_event_session_id(event))
    }

    fn ui_session_id_for_event(&self, event: &Event) -> Option<SessionId> {
        match event {
            Event::UiPromptSubmitted(prompt) => Some(prompt.session_id.clone()),
            Event::UiShellCommand(command) => Some(command.session_id.clone()),
            Event::UiSwitchSession(req) => Some(req.new_session_id.clone()),
            Event::UiNewAgent(req) => Some(req.session_id.clone()),
            Event::UiAgentStateRequest(req) => Some(req.session_id.clone()),
            Event::UiTreeRequest(req) => Some(req.session_id.clone()),
            Event::UiNavigateTree(req) => Some(req.session_id.clone()),
            Event::UiCompactRequest(req) => Some(req.session_id.clone()),
            Event::UiCancelPrompt(req) => Some(req.session_id.clone()),
            _ => None,
        }
    }

    fn session_event_session_id(&self, event: &Event) -> Option<SessionId> {
        match event {
            Event::SessionPromptQueued(queued) => Some(queued.session_id.clone()),
            Event::SessionPromptRecalled(recalled) => Some(recalled.session_id.clone()),
            Event::SessionPromptSteered(steered) => Some(steered.session_id.clone()),
            Event::SessionStarted(started) => Some(started.session_id.clone()),
            Event::SessionShutdown(shutdown) => Some(shutdown.session_id.clone()),
            Event::SessionAgentStateChanged(changed) => Some(changed.session_id.clone()),
            Event::SessionCompactionStarted(started) => Some(started.session_id.clone()),
            Event::SessionCompactionFinished(finished) => Some(finished.session_id.clone()),
            Event::SessionCompacted(compacted) => Some(compacted.session_id.clone()),
            Event::SessionCompactionRequested(requested) => {
                Some(requested.prompt.session_id.clone())
            }
            Event::SessionPromptCreated(created) => Some(created.session_id.clone()),
            Event::SessionPromptTerminated(terminated) => Some(terminated.session_id.clone()),
            Event::SessionPromptPrewarmRequested(prewarm) => Some(prewarm.session_id.clone()),
            Event::SessionUserMessageInjected(injected) => Some(injected.session_id.clone()),
            Event::AgentMessage(message) => Some(message.session_id.clone()),
            Event::ShellCommandFinished(finished) => Some(finished.session_id.clone()),
            _ => None,
        }
    }

    fn provider_event_session_id(&self, event: &Event) -> Option<SessionId> {
        match event {
            Event::ProviderPromptSubmitted(submitted) => {
                self.session_id_for_prompt(&submitted.session_prompt_id)
            }
            Event::ProviderResponseUpdated(updated) => {
                self.session_id_for_prompt(&updated.session_prompt_id)
            }
            Event::ProviderResponseFinished(finished) => {
                self.session_id_for_prompt(&finished.session_prompt_id)
            }
            _ => None,
        }
    }

    fn tool_event_session_id(&self, event: &Event) -> Option<SessionId> {
        match event {
            Event::ToolRequest(request) => self.session_id_for_tool_call(&request.call_id),
            Event::ToolStarted(started) => self.session_id_for_tool_call(&started.call_id),
            Event::ToolRejected(rejected) => self.session_id_for_tool_call(&rejected.call_id),
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                self.session_id_for_tool_call(&result.call_id)
            }
            Event::ToolError(error) | Event::ProviderToolError(error) => {
                self.session_id_for_tool_call(&error.call_id)
            }
            Event::ToolBackgroundResult(result) => self.session_id_for_tool_call(&result.call_id),
            Event::ToolBackgroundError(error) => self.session_id_for_tool_call(&error.call_id),
            Event::ToolCancelled(cancelled) => self.session_id_for_tool_call(&cancelled.call_id),
            Event::ToolProgress(progress) => self.session_id_for_tool_call(&progress.call_id),
            Event::ToolDelegateProgress(progress) => {
                self.session_id_for_tool_call(&progress.call_id)
            }
            _ => None,
        }
    }

    fn extension_event_session_id(&self, event: &Event) -> Option<SessionId> {
        match event {
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_) => {
                Some(self.current_session_id.clone())
            }
            Event::ExtSessionContextPublish(publish) => Some(publish.session_id.clone()),
            Event::ExtensionEvent(event) => event.session_id.clone(),
            _ => None,
        }
    }

    fn enable_debug_log(&mut self, dir: &Path) -> Result<PathBuf, HarnessError> {
        let log = DebugEventLog::open(dir)?;
        let path = log.path().to_path_buf();
        self.debug_log = Some(log);
        Ok(path)
    }

    // -----------------------------------------------------------------------
    // Startup
    // -----------------------------------------------------------------------

    /// Drives the event loop until the in-flight session initialization
    /// completes (turn state returns to `Idle`). Called at harness
    /// startup after the eager `start_session_init` for the default
    /// session — see that call site for the design rationale.
    fn wait_for_session_init(&mut self) -> Result<(), HarnessError> {
        if self.turn_state.is_idle() {
            return Ok(());
        }
        let started_at = Instant::now();
        while !self.turn_state.is_idle() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let harness_evt = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    self.handle_extension_event(&connection_id, *frame)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let was_provider = self.is_provider_extension(&connection_id);
                    self.handle_disconnect(&connection_id);
                    if was_provider {
                        return Err(provider_disconnected_error());
                    }
                }
                HarnessEvent::NewClient(_) => {}
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
        Ok(())
    }

    /// Drives the event loop until every configured extension reaches
    /// `ExtensionState::Ready`. Replaces the old `wait_for_startup(n)`:
    /// state transitions are tracked per-extension so the same predicate
    /// can also gate runtime dispatch in `dispatch_blocked_for`.
    fn wait_for_extensions_ready(&mut self) -> Result<(), HarnessError> {
        if self.pending_extension_connects == 0 && self.extensions_all_ready() {
            return Ok(());
        }
        let started_at = Instant::now();
        while self.pending_extension_connects != 0 || !self.extensions_all_ready() {
            let remaining = STARTUP_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let harness_evt = self
                .rx
                .recv_timeout(remaining)
                .map_err(|_| HarnessError::StartupTimeout)?;
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    self.handle_extension_event(&connection_id, *frame)?;
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let name = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| connection_id.to_string());
                    self.handle_disconnect(&connection_id);
                    return Err(HarnessError::Participant(format!(
                        "{name} disconnected during startup"
                    )));
                }
                HarnessEvent::NewClient(_) => {}
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Main event loop (daemon mode)
    // -----------------------------------------------------------------------

    pub(crate) fn run_event_loop(
        &mut self,
        max_clients: Option<usize>,
        mut exit_on_disconnect: bool,
    ) -> Result<(), HarnessError> {
        let mut served_clients = 0_usize;
        let mut ever_attached = false;
        loop {
            if max_clients.is_some_and(|max| served_clients >= max) {
                break;
            }
            // `exit_on_disconnect`: once at least one UI has been
            // attached, exiting the moment the last one leaves lets
            // `tau` behave like a normal foreground command.
            // Before any UI attaches we wait — otherwise a slightly
            // late first connect would race us into immediate exit.
            if exit_on_disconnect && ever_attached && self.client_writers.is_empty() {
                break;
            }
            self.process_background_deadlines();
            let harness_evt = if let Some(deadline) = self.tool_turn.next_background_deadline() {
                let timeout = deadline.saturating_duration_since(Instant::now());
                match self.rx.recv_timeout(timeout) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.process_background_deadlines();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                let Ok(event) = self.rx.recv() else {
                    break;
                };
                event
            };
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    let origin = self
                        .bus
                        .connection(&connection_id)
                        .map(|m| m.origin.clone());
                    match origin {
                        Some(ConnectionOrigin::Socket) => {
                            // `/detach` → stay alive even after this
                            // UI leaves; a later `tau --attach`
                            // can pick up right here.
                            if matches!(frame.as_ref(), Frame::Event(Event::UiDetachRequest(_))) {
                                exit_on_disconnect = false;
                            }
                            let keep = self.handle_client_event(&connection_id, *frame)?;
                            if !keep {
                                let _ = self.bus.disconnect(&connection_id);
                                served_clients += 1;
                            }
                        }
                        Some(_) => self.handle_extension_event(&connection_id, *frame)?,
                        None => {} // already disconnected
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let was_provider = self.is_provider_extension(&connection_id);
                    let was_socket = self
                        .bus
                        .connection(&connection_id)
                        .is_some_and(|m| m.origin == ConnectionOrigin::Socket);
                    self.handle_disconnect(&connection_id);
                    if was_socket {
                        served_clients += 1;
                    }
                    if was_provider {
                        return Err(provider_disconnected_error());
                    }
                }
                HarnessEvent::NewClient(stream) => {
                    self.accept_client(stream)?;
                    ever_attached = true;
                }
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Client acceptance
    // -----------------------------------------------------------------------

    fn accept_client(&mut self, stream: UnixStream) -> Result<(), HarnessError> {
        let write_stream = stream.try_clone()?;
        let writer_tx = spawn_writer_thread(write_stream, WriterShutdown::CloseStream);
        let writer_tx_for_follower = writer_tx.clone();
        let conn_id = self.bus.connect(Connection::new(
            ConnectionMetadata {
                id: tau_proto::ConnectionId::default(),
                name: "socket-ui".to_owned(),
                kind: ClientKind::Ui,
                origin: ConnectionOrigin::Socket,
            },
            Box::new(ChannelSink { tx: writer_tx }),
        ));
        self.client_writers
            .insert(conn_id.clone(), writer_tx_for_follower);
        spawn_reader_thread(conn_id, stream, self.tx.clone());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn handle_extension_event(
        &mut self,
        source_id: &str,
        frame: Frame,
    ) -> Result<(), HarnessError> {
        match frame {
            Frame::Message(msg) => self.handle_extension_message(source_id, msg),
            Frame::Event(event) => self.handle_extension_event_inner(source_id, event),
        }
    }

    fn send_session_prompt_created_result(
        &mut self,
        connection_id: &str,
        request: tau_proto::GetSessionPromptCreated,
    ) {
        let prompt = self
            .prompt_snapshots
            .get(&request.session_prompt_id)
            .cloned()
            .or_else(|| {
                self.read_session_prompt_created(&request.session_prompt_id)
                    .ok()
            });
        let _ = self.bus.send_to(
            connection_id,
            None,
            Frame::Message(Message::SessionPromptCreatedResult(Box::new(
                tau_proto::SessionPromptCreatedResult {
                    request_id: request.request_id,
                    prompt,
                },
            ))),
        );
    }

    fn send_rendered_system_prompt_result(
        &mut self,
        connection_id: &str,
        request: tau_proto::GetRenderedSystemPrompt,
    ) {
        let (prompt, error) = if !self.available_roles.contains_key(&request.role) {
            (None, Some(format!("unknown role: {}", request.role)))
        } else {
            (Some(self.build_system_prompt_for_role(&request.role)), None)
        };
        let _ = self.bus.send_to(
            connection_id,
            None,
            Frame::Message(Message::RenderedSystemPromptResult(Box::new(
                tau_proto::RenderedSystemPromptResult {
                    request_id: request.request_id,
                    prompt,
                    error,
                },
            ))),
        );
    }

    fn send_rendered_tool_definitions_result(
        &mut self,
        connection_id: &str,
        request: tau_proto::GetRenderedToolDefinitions,
    ) {
        let (tools, error) = if !self.available_roles.contains_key(&request.role) {
            (None, Some(format!("unknown role: {}", request.role)))
        } else {
            (
                Some(self.gather_tool_definitions_for_role(&request.role)),
                None,
            )
        };
        let _ = self.bus.send_to(
            connection_id,
            None,
            Frame::Message(Message::RenderedToolDefinitionsResult(Box::new(
                tau_proto::RenderedToolDefinitionsResult {
                    request_id: request.request_id,
                    tools,
                    error,
                },
            ))),
        );
    }

    fn should_stage_extension_capabilities(&self, source_id: &str) -> bool {
        self.extensions
            .get(source_id)
            .is_some_and(|entry| entry.state != ExtensionState::Ready)
    }

    fn extension_activation_stage_mut(&mut self, source_id: &str) -> &mut ExtensionActivationStage {
        self.extension_activation_staging
            .entry(source_id.into())
            .or_default()
    }

    fn stage_extension_tool_registration(&mut self, source_id: &str, registration: ToolRegister) {
        self.extension_activation_stage_mut(source_id)
            .tool_registrations
            .push(registration);
    }

    fn remove_staged_tool_registration(&mut self, source_id: &str, tool_name: &ToolName) -> bool {
        let Some(stage) = self.extension_activation_staging.get_mut(source_id) else {
            return false;
        };
        let before = stage.tool_registrations.len();
        stage
            .tool_registrations
            .retain(|registration| registration.tool.name != *tool_name);
        stage.tool_registrations.len() != before
    }

    fn stage_provider_models_update(
        &mut self,
        source_id: &str,
        update: tau_proto::ProviderModelsUpdated,
    ) {
        self.extension_activation_stage_mut(source_id)
            .provider_model_updates
            .push(update);
    }

    fn stage_extension_skill_available(
        &mut self,
        source_id: &str,
        skill: tau_proto::ExtSkillAvailable,
    ) {
        self.extension_activation_stage_mut(source_id)
            .skill_announcements
            .push(skill);
    }

    fn stage_agents_md_available(
        &mut self,
        source_id: &str,
        agents: tau_proto::ExtAgentsMdAvailable,
    ) {
        self.extension_activation_stage_mut(source_id)
            .agents_files
            .push(agents);
    }

    fn stage_session_context_publish(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtSessionContextPublish,
    ) {
        self.extension_activation_stage_mut(source_id)
            .session_context_publishes
            .push(publish);
    }

    fn stage_extension_prompt_fragment(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtPromptFragmentPublish,
    ) {
        self.extension_activation_stage_mut(source_id)
            .prompt_fragments
            .insert(publish.fragment.name.clone(), publish.fragment);
    }

    fn stage_extension_intercept(&mut self, source_id: &str, intercept: tau_proto::Intercept) {
        self.extension_activation_stage_mut(source_id).intercept = Some(intercept);
    }

    fn stage_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) {
        self.extension_activation_stage_mut(source_id)
            .context_ready_events
            .push(ready);
    }

    fn stage_start_agent_request(&mut self, source_id: &str, query: tau_proto::StartAgentRequest) {
        self.extension_activation_stage_mut(source_id)
            .agent_queries
            .push(query);
    }

    fn stage_extension_publish(&mut self, source_id: &str, event: Event, transient: bool) {
        self.extension_activation_stage_mut(source_id)
            .emitted_events
            .push(StagedExtensionPublish { event, transient });
    }

    fn stage_action_schema(&mut self, source_id: &str, schema: tau_actions::ActionSchema) {
        self.extension_activation_stage_mut(source_id).action_schema = Some(schema);
    }

    fn register_extension_tool(&mut self, source_id: &str, registration: ToolRegister) {
        let internal_name = registration.tool.name.clone();
        let visible_name = self.tool_model_visible_name(&registration.tool).clone();
        let was_available = !self
            .registry
            .providers_for(internal_name.as_str())
            .is_empty();
        let _ = self
            .registry
            .register_with_prompt_fragment(source_id, registration);
        self.ensure_tool_started_subscription(source_id);
        if !was_available {
            self.mark_tool_available_for_notice(internal_name, visible_name);
        }
    }

    fn ensure_tool_started_subscription(&mut self, source_id: &str) {
        let selector = EventSelector::Exact(tau_proto::EventName::TOOL_STARTED);
        let mut selectors = self
            .bus
            .subscriptions(source_id)
            .map_or_else(Vec::new, |s| s.to_vec());
        if selectors.iter().any(|existing| existing == &selector) {
            return;
        }
        selectors.push(selector);
        if let Err(error) = self.bus.set_subscriptions(source_id, selectors) {
            tracing::warn!(
                target: "tau_harness",
                connection_id = %source_id,
                %error,
                "could not subscribe tool provider to tool.started"
            );
        }
    }

    fn register_extension_interceptor(&mut self, source_id: &str, intercept: tau_proto::Intercept) {
        let component_name = self
            .bus
            .connection(source_id)
            .map(|m| ExtensionName::from(m.name.clone()))
            .unwrap_or_else(|| ExtensionName::from(source_id.to_owned()));
        self.interceptors.replace_for_connection(
            source_id,
            component_name,
            intercept.selectors,
            intercept.priority,
        );
    }

    fn publish_extension_prompt_fragment(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtPromptFragmentPublish,
    ) {
        let contributor = tau_proto::ConnectionId::from(source_id);
        self.extension_prompt_fragments
            .entry(contributor)
            .or_default()
            .insert(publish.fragment.name.clone(), publish.fragment.clone());
        self.publish_event(Some(source_id), Event::ExtPromptFragmentPublish(publish));
    }

    fn publish_extension_skill_available(
        &mut self,
        source_id: &str,
        skill: tau_proto::ExtSkillAvailable,
    ) {
        self.record_discovered_skill(source_id, &skill);
        self.publish_event(Some(source_id), Event::ExtSkillAvailable(skill));
    }

    fn publish_agents_md_available(
        &mut self,
        source_id: &str,
        agents: tau_proto::ExtAgentsMdAvailable,
    ) {
        let file_path = PathBuf::from(&agents.file_path);
        if let Some(existing) = self
            .discovered_agents_files
            .iter_mut()
            .find(|existing| existing.source_id == source_id && existing.file_path == file_path)
        {
            existing.content = agents.content.clone();
        } else {
            self.discovered_agents_files.push(DiscoveredAgentsFile {
                source_id: source_id.into(),
                file_path,
                content: agents.content.clone(),
            });
        }
        self.publish_event(Some(source_id), Event::ExtAgentsMdAvailable(agents));
    }

    fn publish_provider_models_update(
        &mut self,
        source_id: &str,
        update: tau_proto::ProviderModelsUpdated,
    ) {
        self.publish_event(
            Some(source_id),
            Event::ProviderModelsUpdated(update.clone()),
        );
        self.set_provider_models(source_id, update.models);
    }

    fn extension_action_owner(
        &self,
        source_id: &str,
    ) -> (ExtensionName, tau_proto::ExtensionInstanceId) {
        if let Some(extension) = self.extensions.get(source_id) {
            return (
                ExtensionName::from(extension.name.clone()),
                extension.instance_id,
            );
        }
        self.bus.connection(source_id).map_or_else(
            || (ExtensionName::from(source_id.to_owned()), 0.into()),
            |metadata| (ExtensionName::from(metadata.name.clone()), 0.into()),
        )
    }

    fn publish_action_schema(&mut self, source_id: &str, schema: tau_actions::ActionSchema) {
        let (extension_name, instance_id) = self.extension_action_owner(source_id);
        if let Err(error) = self.action_registry.register_schema(
            source_id,
            extension_name.clone(),
            instance_id,
            schema.clone(),
        ) {
            self.emit_info(&format!(
                "extension {extension_name} published invalid action schema: {error}"
            ));
            return;
        }
        self.publish_event(
            Some(source_id),
            Event::ActionSchemaPublished(ActionSchemaPublished {
                extension_name,
                instance_id,
                schema,
            }),
        );
    }

    fn publish_session_context_publish(
        &mut self,
        source_id: &str,
        publish: tau_proto::ExtSessionContextPublish,
    ) {
        let contributor = tau_proto::ConnectionId::from(source_id);
        let extension_name = self
            .extensions
            .get(&contributor)
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| source_id.to_owned());
        self.session_context.publish(
            publish.session_id.clone(),
            publish.key.clone(),
            contributor,
            extension_name,
            publish.value.clone(),
        );
        self.publish_event(Some(source_id), Event::ExtSessionContextPublish(publish));
    }

    fn publish_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        self.publish_event(Some(source_id), Event::ExtensionContextReady(ready.clone()));
        self.handle_extension_context_ready(source_id, ready)
    }

    fn activate_staged_extension_capabilities(
        &mut self,
        source_id: &str,
    ) -> (
        Vec<tau_proto::ExtensionContextReady>,
        Vec<tau_proto::StartAgentRequest>,
    ) {
        let Some(stage) = self.extension_activation_staging.remove(source_id) else {
            return (Vec::new(), Vec::new());
        };
        if let Some(intercept) = stage.intercept {
            self.register_extension_interceptor(source_id, intercept);
        }
        for registration in stage.tool_registrations {
            self.register_extension_tool(source_id, registration);
        }
        for update in stage.provider_model_updates {
            self.publish_provider_models_update(source_id, update);
        }
        if let Some(schema) = stage.action_schema {
            self.publish_action_schema(source_id, schema);
        }
        for skill in stage.skill_announcements {
            self.publish_extension_skill_available(source_id, skill);
        }
        for agents in stage.agents_files {
            self.publish_agents_md_available(source_id, agents);
        }
        for publish in stage.session_context_publishes {
            self.publish_session_context_publish(source_id, publish);
        }
        for fragment in stage.prompt_fragments.into_values() {
            self.publish_extension_prompt_fragment(
                source_id,
                tau_proto::ExtPromptFragmentPublish { fragment },
            );
        }
        for staged in stage.emitted_events {
            self.enqueue_publish(Some(source_id), staged.event, staged.transient, false, None);
        }
        (stage.context_ready_events, stage.agent_queries)
    }

    fn handle_extension_message(
        &mut self,
        source_id: &str,
        message: Message,
    ) -> Result<(), HarnessError> {
        match message {
            Message::Ack(ack) => {
                // Cumulative ack: advance the cursor if it moves
                // forward, ignore otherwise (duplicates, late acks).
                if let Some(entry) = self.extensions.get_mut(source_id)
                    && entry.last_acked.get() < ack.up_to.get()
                {
                    entry.last_acked = ack.up_to;
                }
            }
            Message::Hello(_hello) => {
                self.set_extension_state(source_id, ExtensionState::Handshaking);
                self.send_lifecycle_configure(source_id);
            }
            Message::ConfigError(err) => {
                let name = self
                    .extensions
                    .get(source_id)
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| "extension".to_owned());
                self.emit_info_important(&format!(
                    "extension {name} rejected its config: {}\ncheck \
                     `extensions.{name}.config` and `extensions.{name}.secrets` in harness.yaml; \
                     invalid values are being ignored",
                    err.message,
                ));
            }
            Message::Subscribe(subscribe) => {
                // Extension subscriptions are live-only today: set routing for
                // future events, without replaying past log entries. Do not
                // treat first-party extensions that want live-only delivery as
                // universal; any external-extension replay support needs an
                // explicit opt-in separate from selectors.
                self.bus.set_subscriptions(source_id, subscribe.selectors)?;
            }
            Message::Intercept(intercept) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_intercept(source_id, intercept);
                } else {
                    self.register_extension_interceptor(source_id, intercept);
                }
            }
            Message::Ready(_ready) => {
                let (context_ready_events, agent_queries) =
                    self.activate_staged_extension_capabilities(source_id);
                self.set_extension_state(source_id, ExtensionState::Ready);
                self.emit_extension_ready(source_id);
                for ready in context_ready_events {
                    self.publish_extension_context_ready(source_id, ready)?;
                }
                for query in agent_queries {
                    self.handle_start_agent_request(source_id, query)?;
                }
                self.drain_pending_tool_invocations()?;
                self.try_advance_queue();
            }
            Message::Emit(emit) => {
                let event = *emit.event;
                if matches!(
                    event,
                    Event::ActionSchemaPublished(_)
                        | Event::ActionInvoke(_)
                        | Event::ActionResult(_)
                        | Event::ActionError(_)
                ) {
                    self.handle_extension_event_inner(source_id, event)?;
                    return Ok(());
                }
                if event.name().category == tau_proto::EventCategory::Provider
                    || Self::requires_tool_event_intake(&event)
                    || matches!(event, Event::AgentMessage(_))
                {
                    return Ok(());
                }
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_publish(source_id, event, emit.transient);
                } else {
                    self.enqueue_publish(Some(source_id), event, emit.transient, false, None);
                }
            }
            Message::InterceptReply(reply) => {
                self.handle_intercept_reply(source_id, reply);
            }
            Message::GetSessionPromptCreated(request) => {
                self.send_session_prompt_created_result(source_id, request);
            }
            // Messages sent by clients or the harness only — extensions shouldn't
            // round-trip these. Ignore silently.
            Message::Configure(_)
            | Message::Disconnect(_)
            | Message::GetRenderedSystemPrompt(_)
            | Message::GetRenderedToolDefinitions(_)
            | Message::InterceptRequest(_)
            | Message::SessionPromptCreatedResult(_)
            | Message::RenderedSystemPromptResult(_)
            | Message::RenderedToolDefinitionsResult(_)
            | Message::LogEvent(_) => {}
        }
        Ok(())
    }

    fn handle_extension_event_inner(
        &mut self,
        source_id: &str,
        event: Event,
    ) -> Result<(), HarnessError> {
        let event_name = event.name();
        if event_name.category == tau_proto::EventCategory::Provider
            && !self.accepts_provider_event_from(source_id, &event_name)
        {
            return Ok(());
        }

        match event {
            Event::ToolRegister(registration) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_tool_registration(source_id, registration);
                } else {
                    self.register_extension_tool(source_id, registration);
                }
            }
            Event::ActionSchemaPublished(published) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_action_schema(source_id, published.schema);
                } else {
                    self.publish_action_schema(source_id, published.schema);
                }
            }
            Event::ActionResult(result) => {
                self.handle_action_result(source_id, result);
            }
            Event::ActionError(error) => {
                self.handle_action_error(source_id, error);
            }
            Event::ActionInvoke(_) => {}
            Event::ToolUnregister(unregister) => {
                self.remove_staged_tool_registration(source_id, &unregister.tool_name);
                if self.should_stage_extension_capabilities(source_id) {
                    return Ok(());
                }
                let visible_name = self
                    .registry
                    .providers_for(unregister.tool_name.as_str())
                    .into_iter()
                    .find(|provider| provider.connection_id.as_str() == source_id)
                    .map(|provider| self.tool_model_visible_name(&provider.tool).clone())
                    .unwrap_or_else(|| unregister.tool_name.clone());
                let removed = self
                    .registry
                    .unregister(source_id, unregister.tool_name.as_str());
                if removed
                    && self
                        .registry
                        .providers_for(unregister.tool_name.as_str())
                        .is_empty()
                {
                    self.mark_tool_unavailable_for_notice(
                        unregister.tool_name.clone(),
                        visible_name,
                    );
                }
                self.publish_event(Some(source_id), Event::ToolUnregister(unregister));
            }
            Event::ToolRequest(request) => {
                // Track session attribution before publishing — the
                // publish path's `session_id_for_event` reads
                // `pending_tool_sessions` to attach the persisted
                // record to the right session.
                self.track_tool_request_session(&request);
                // Stamp the publish with the owning conversation so
                // the fold lands on its branch. Without this, after
                // Phase 4 of the interception refactor (no global
                // head-bouncing), a sibling conversation that
                // recently appended would leave `tree.head` on its
                // own tip, and the tool-request node would fold
                // there instead.
                let owning_cid = self.tool_conversations.get(&request.call_id).cloned();
                if let Some(cid) = owning_cid.as_ref()
                    && !self.pending_tools.contains_key(&request.call_id)
                {
                    self.pending_tools.insert(
                        request.call_id.clone(),
                        PendingTool {
                            name: request.tool_name.clone(),
                            internal_name: request.tool_name.clone(),
                            tool_type: request.tool_type,
                        },
                    );
                    self.bump_tools_started_for(cid);
                }
                let event = Event::ToolRequest(request.clone());
                match owning_cid.as_ref() {
                    Some(cid) => self.publish_event_for_conversation(cid, Some(source_id), event),
                    None => self.publish_event(Some(source_id), event),
                }
                // `ToolRequest` is the persisted pre-routing intent.
                // `route_tool_request` resolves it. On success we
                // publish durable `ToolStarted`; subscribed tool
                // extensions see that event and the owner starts work. On
                // failure we publish `ToolRejected` and the terminal
                // `ToolError` for model-facing completion.
                match self.registry.route_tool_request(request.clone()) {
                    Ok(route) => {
                        let started = route.invoke;
                        let event = Event::ToolStarted(started.clone());
                        match owning_cid.as_ref() {
                            Some(cid) => {
                                self.publish_for_conversation_from(cid, Some(source_id), event)
                            }
                            None => self.publish_event(Some(source_id), event),
                        }
                        match route.target {
                            ToolRouteTarget::Internal => {}
                            ToolRouteTarget::Extension(provider_connection_id) => {
                                self.ensure_tool_started_subscription(&provider_connection_id);
                                self.pending_tool_providers
                                    .insert(request.call_id.clone(), provider_connection_id);
                            }
                        }
                    }
                    Err(ToolRouteError::NoProvider { tool_name }) => {
                        let call_id = request.call_id.to_string();
                        let owning_cid = self.tool_conversations.get(&request.call_id).cloned();
                        let message = unavailable_tool_error_message(&tool_name);
                        let rejected = ToolRejected {
                            call_id: request.call_id.clone(),
                            tool_name: tool_name.clone(),
                            tool_type: request.tool_type,
                            message: message.clone(),
                            originator: request.originator.clone(),
                        };
                        let event = Event::ToolRejected(rejected);
                        match owning_cid.as_ref() {
                            Some(cid) => {
                                self.publish_for_conversation_from(cid, Some(source_id), event)
                            }
                            None => self.publish_event(Some(source_id), event),
                        }
                        let error = ToolError {
                            call_id: request.call_id,
                            tool_name: tool_name.clone(),
                            tool_type: request.tool_type,
                            message,
                            details: None,
                            display: None,
                            originator: tau_proto::PromptOriginator::User,
                        };
                        self.publish_terminal_tool_error(owning_cid.as_ref(), None, error);
                        self.clear_tool_call_tracking(&call_id);
                    }
                    Err(error) => return Err(HarnessError::ToolRoute(error)),
                }
            }
            Event::ToolResult(mut result) => {
                if !self.validate_tool_event_source(&result.call_id, source_id) {
                    return Ok(());
                }
                if self.tool_turn.is_backgrounded(&result.call_id) {
                    self.handle_background_tool_result(source_id, result);
                } else if let Some(cid) = self.tool_conversations.get(&result.call_id).cloned() {
                    let call_id = result.call_id.to_string();
                    if let Some(tool) = self.pending_tools.get(&result.call_id) {
                        result.tool_name = tool.name.clone();
                        result.tool_type = tool.tool_type;
                    }
                    // Collapse byte-identical large results into a
                    // pointer back to the first call_id that produced
                    // this content on this conversation's branch. See
                    // `crate::dedup` for the design.
                    self.dedup_tool_result(&cid, &mut result);
                    // Snap to the owning conversation's head before
                    // folding the result. Without this, a sibling side
                    // conv that just ran `snap_to_default_conversation`
                    // (during its teardown) leaves `tree.head` on the
                    // *parent* branch — folding the result there
                    // misplaces it and produces orphan ToolUse blocks
                    // when the parent conv is later re-prompted.
                    self.publish_terminal_tool_result(Some(&cid), Some(source_id), result);
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool result for call_id={}",
                        result.call_id
                    ));
                }
            }
            Event::ToolError(mut error) => {
                if !self.validate_tool_event_source(&error.call_id, source_id) {
                    return Ok(());
                }
                if self.tool_turn.is_backgrounded(&error.call_id) {
                    self.handle_background_tool_error(Some(source_id), error);
                } else if let Some(cid) = self.tool_conversations.get(&error.call_id).cloned() {
                    let call_id = error.call_id.to_string();
                    if let Some(tool) = self.pending_tools.get(&error.call_id) {
                        error.tool_name = tool.name.clone();
                        error.tool_type = tool.tool_type;
                    }
                    self.dedup_tool_error(&cid, &mut error);
                    self.publish_terminal_tool_error(Some(&cid), Some(source_id), error);
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
                } else {
                    self.emit_info(&format!(
                        "discarding duplicate tool error for call_id={}",
                        error.call_id
                    ));
                }
            }
            Event::ToolProgress(progress) => {
                if !self.tool_conversations.contains_key(&progress.call_id)
                    || !self.validate_tool_event_source(&progress.call_id, source_id)
                {
                    return Ok(());
                }
                if !self.tool_turn.is_backgrounded(&progress.call_id) {
                    self.publish_event(Some(source_id), Event::ToolProgress(progress));
                }
            }
            Event::ProviderToolResult(_) | Event::ProviderToolError(_) | Event::AgentMessage(_) => {
                return Ok(());
            }
            Event::ToolCancelled(mut cancelled) => {
                if !self.validate_tool_event_source(&cancelled.call_id, source_id) {
                    return Ok(());
                }
                if self.tool_turn.is_backgrounded(&cancelled.call_id) {
                    self.handle_background_tool_cancelled(source_id, cancelled);
                } else if let Some(cid) = self.tool_conversations.get(&cancelled.call_id).cloned() {
                    let call_id = cancelled.call_id.to_string();
                    if let Some(tool) = self.pending_tools.get(&cancelled.call_id) {
                        cancelled.tool_name = tool.name.clone();
                        cancelled.tool_type = tool.tool_type;
                    }
                    self.publish_for_conversation_from(
                        &cid,
                        Some(source_id),
                        Event::ToolCancelled(cancelled),
                    );
                    self.on_tool_call_complete(&call_id);
                    self.clear_tool_call_tracking(&call_id);
                }
            }
            Event::ToolBackgroundResult(_) | Event::ToolBackgroundError(_)
                if source_id != HARNESS_CONNECTION_ID =>
            {
                return Ok(());
            }
            Event::ShellCommandProgress(progress) => {
                // Pass-through: the UI renders chunks as they arrive.
                self.publish_event(Some(source_id), Event::ShellCommandProgress(progress));
            }
            Event::ShellCommandFinished(finished) => {
                // Publish first so the UI finalizes its render block
                // regardless of whether we inject into history.
                self.publish_event(
                    Some(source_id),
                    Event::ShellCommandFinished(finished.clone()),
                );
                if finished.include_in_context {
                    self.inject_user_shell_output(&finished);
                }
            }
            Event::ExtSkillAvailable(skill) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_skill_available(source_id, skill);
                } else {
                    self.publish_extension_skill_available(source_id, skill);
                }
            }
            Event::ExtAgentsMdAvailable(agents) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_agents_md_available(source_id, agents);
                } else {
                    self.publish_agents_md_available(source_id, agents);
                }
            }
            Event::ProviderModelsUpdated(updated) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_provider_models_update(source_id, updated);
                } else {
                    self.publish_provider_models_update(source_id, updated);
                }
            }
            Event::ExtensionContextReady(ready) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_context_ready(source_id, ready);
                } else {
                    self.publish_extension_context_ready(source_id, ready)?;
                }
            }
            Event::ExtSessionContextPublish(publish) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_session_context_publish(source_id, publish);
                } else {
                    self.publish_session_context_publish(source_id, publish);
                }
            }
            Event::ExtPromptFragmentPublish(publish) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_extension_prompt_fragment(source_id, publish);
                } else {
                    self.publish_extension_prompt_fragment(source_id, publish);
                }
            }
            Event::StartAgentRequest(query) => {
                if self.should_stage_extension_capabilities(source_id) {
                    self.stage_start_agent_request(source_id, query);
                } else {
                    self.handle_start_agent_request(source_id, query)?;
                }
            }
            Event::ProviderPromptSubmitted(submitted) => {
                if !self.canceled_prompts.contains(&submitted.session_prompt_id)
                    && self.provider_prompt_owner_matches(
                        source_id,
                        &submitted.session_prompt_id,
                        tau_proto::EventName::PROVIDER_PROMPT_SUBMITTED,
                    )
                {
                    self.publish_event(Some(source_id), Event::ProviderPromptSubmitted(submitted));
                }
            }
            Event::ProviderResponseUpdated(updated) => {
                if !self.canceled_prompts.contains(&updated.session_prompt_id)
                    && self.provider_prompt_owner_matches(
                        source_id,
                        &updated.session_prompt_id,
                        tau_proto::EventName::PROVIDER_RESPONSE_UPDATED,
                    )
                {
                    self.publish_event(Some(source_id), Event::ProviderResponseUpdated(updated));
                }
            }
            Event::ProviderResponseFinished(response) => {
                if self.provider_prompt_owner_matches(
                    source_id,
                    &response.session_prompt_id,
                    tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
                ) {
                    self.handle_provider_response_finished_from(Some(source_id), response)?;
                }
            }
            other => {
                if self.should_stage_extension_capabilities(source_id) {
                    let transient = other.defaults_to_transient();
                    self.stage_extension_publish(source_id, other, transient);
                } else {
                    self.publish_event(Some(source_id), other);
                }
            }
        }
        Ok(())
    }

    fn handle_client_event(&mut self, client_id: &str, frame: Frame) -> Result<bool, HarnessError> {
        match frame {
            Frame::Message(msg) => self.handle_client_message(client_id, msg),
            Frame::Event(event) => self.handle_client_event_inner(client_id, event),
        }
    }

    fn handle_client_message(
        &mut self,
        client_id: &str,
        message: Message,
    ) -> Result<bool, HarnessError> {
        match message {
            Message::Hello(_hello) => Ok(true),
            Message::Subscribe(subscribe) => {
                // Socket/UI clients replay selected past state after subscribing.
                // Extensions use `handle_extension_message`, which is live-only.
                match self
                    .bus
                    .set_subscriptions(client_id, subscribe.selectors.clone())
                {
                    Ok(()) => {
                        let selectors_for_replay = subscribe.selectors;
                        self.replay_session_events(client_id, &selectors_for_replay);
                        self.replay_harness_info(client_id, &selectors_for_replay);
                        Ok(true)
                    }
                    Err(RouteError::SubscriptionDenied { reason, .. }) => {
                        let _ = self.bus.send_to(
                            client_id,
                            None,
                            Frame::Message(Message::Disconnect(Disconnect {
                                reason: Some(format!("subscription denied: {reason}")),
                            })),
                        );
                        Ok(false)
                    }
                    Err(other) => Err(HarnessError::Route(other)),
                }
            }
            Message::Disconnect(_) => Ok(false),
            Message::GetSessionPromptCreated(request) => {
                self.send_session_prompt_created_result(client_id, request);
                Ok(true)
            }
            Message::GetRenderedSystemPrompt(request) => {
                self.send_rendered_system_prompt_result(client_id, request);
                Ok(true)
            }
            Message::GetRenderedToolDefinitions(request) => {
                self.send_rendered_tool_definitions_result(client_id, request);
                Ok(true)
            }
            // Other messages from clients are ignored (Configure, Ack,
            // LogEvent, InterceptRequest, InterceptReply, Emit,
            // ConfigError, Intercept).
            Message::Ack(_)
            | Message::Configure(_)
            | Message::ConfigError(_)
            | Message::Intercept(_)
            | Message::InterceptRequest(_)
            | Message::InterceptReply(_)
            | Message::Ready(_)
            | Message::SessionPromptCreatedResult(_)
            | Message::RenderedSystemPromptResult(_)
            | Message::RenderedToolDefinitionsResult(_)
            | Message::LogEvent(_)
            | Message::Emit(_) => Ok(true),
        }
    }

    fn handle_client_event_inner(
        &mut self,
        client_id: &str,
        event: Event,
    ) -> Result<bool, HarnessError> {
        let event_name = event.name();
        if event_name.category == tau_proto::EventCategory::Provider {
            self.handle_extension_event_inner(client_id, event)?;
            return Ok(true);
        }

        match event {
            Event::UiRoleSelect(select) => self.handle_ui_role_select(select),
            Event::UiRoleUpdate(req) => self.handle_ui_role_update(req),
            Event::UiPromptSubmitted(prompt) => self.handle_ui_prompt_submitted(prompt),
            Event::ActionInvoke(invoke) => self.handle_action_invoke(client_id, invoke),
            Event::ActionSchemaPublished(_) | Event::ActionResult(_) | Event::ActionError(_) => {
                Ok(true)
            }
            Event::UiSwitchSession(req) => self.handle_ui_switch_session(client_id, req),
            Event::UiNewAgent(req) => self.handle_ui_new_agent(req),
            Event::UiAgentStateRequest(req) => self.handle_ui_agent_state_request(req),
            Event::UiTreeRequest(req) => self.handle_ui_tree_request(client_id, req),
            Event::UiNavigateTree(req) => self.handle_ui_navigate_tree(client_id, req),
            Event::UiCompactRequest(req) => self.handle_ui_compact_request(client_id, req),
            Event::UiCancelPrompt(req) => {
                self.handle_cancel_prompt(&req);
                Ok(true)
            }
            Event::UiRecallQueuedPrompt(req) => {
                self.handle_recall_queued_prompt(&req);
                Ok(true)
            }
            other => {
                if Self::requires_tool_event_intake(&other)
                    || matches!(other, Event::AgentMessage(_))
                {
                    return Ok(true);
                }
                self.publish_event(Some(client_id), other);
                Ok(true)
            }
        }
    }

    fn handle_ui_role_select(
        &mut self,
        select: tau_proto::UiRoleSelect,
    ) -> Result<bool, HarnessError> {
        if !self.available_roles.contains_key(&select.role) {
            self.publish_event(
                None,
                Event::HarnessInfo(tau_proto::HarnessInfo {
                    message: format!("unknown role: {}", select.role),
                    level: tau_proto::HarnessInfoLevel::Normal,
                }),
            );
            return Ok(true);
        }

        let was_empty = self.selected_model.is_none();
        self.selected_role = select.role.clone();
        self.reconcile_selected_model_with_available();
        if self.selected_model.is_none() {
            self.publish_event(
                None,
                Event::HarnessInfo(tau_proto::HarnessInfo {
                    message: format!("role `{}` has no available model", select.role),
                    level: tau_proto::HarnessInfoLevel::Normal,
                }),
            );
        }
        self.publish_current_model_state();
        if was_empty && self.selected_model.is_some() && self.turn_state.is_idle() {
            self.try_advance_queue();
        }
        Ok(true)
    }

    fn handle_ui_role_update(
        &mut self,
        req: tau_proto::UiRoleUpdate,
    ) -> Result<bool, HarnessError> {
        let mut selected_role_changed = false;
        let selected_was_empty = self.selected_model.is_none();
        match req.action {
            tau_proto::UiRoleUpdateAction::Delete => {
                selected_role_changed = self.handle_ui_role_delete(req.role)?;
            }
            action => {
                if let Some(next_role) = self.role_after_update(&req.role, action) {
                    self.available_roles
                        .insert(req.role.clone(), next_role.clone());
                    self.role_overrides.insert(req.role.clone(), next_role);
                    selected_role_changed = self.selected_role == req.role;
                }
            }
        }
        if selected_role_changed {
            self.reconcile_selected_model_with_available();
            self.publish_current_model_state();
            if selected_was_empty && self.selected_model.is_some() && self.turn_state.is_idle() {
                self.try_advance_queue();
            }
        }
        self.publish_event(
            None,
            Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                roles: role_infos(
                    &self.provider_model_info,
                    &self.available_roles,
                    &self.available_models,
                ),
                groups: self.current_role_groups(),
            }),
        );
        self.publish_delegate_roles_context();
        Ok(true)
    }

    fn handle_ui_role_delete(&mut self, role_name: String) -> Result<bool, HarnessError> {
        let was_selected = self.selected_role == role_name;
        let previous_override = self.role_overrides.remove(&role_name);
        let configured_role = load_harness_settings_or_warn(&self.dirs)
            .0
            .roles
            .get(&role_name)
            .cloned();

        if let Some(role) = configured_role {
            self.available_roles.insert(role_name, role);
            return Ok(was_selected);
        }

        let removed_role = self.available_roles.remove(&role_name);
        if self.available_roles.is_empty() {
            if let Some(role) = removed_role {
                self.available_roles.insert(role_name.clone(), role);
            }
            if let Some(role) = previous_override {
                self.role_overrides.insert(role_name.clone(), role);
            }
            self.emit_info("/role: cannot delete the last role");
            return Ok(false);
        }
        if was_selected {
            self.selected_role = fallback_role(&self.available_roles);
            return Ok(true);
        }
        Ok(false)
    }

    fn handle_ui_prompt_submitted(
        &mut self,
        prompt: tau_proto::UiPromptSubmitted,
    ) -> Result<bool, HarnessError> {
        if let Some(target_agent_id) = prompt.target_agent_id.as_deref()
            && target_agent_id != "main"
        {
            let submission = self.submit_prompt_to_agent(
                prompt.session_id.clone(),
                target_agent_id,
                prompt.text.clone(),
            )?;
            if matches!(submission, PromptSubmission::Queued) && !prompt.message_class.is_internal()
            {
                self.interrupt_active_waits();
            }
            return Ok(true);
        }

        // Stash the correlation tag on the default conversation
        // before submission; `send_prompt_to_agent_for` will consume
        // it when it constructs the matching `SessionPromptCreated`.
        // Queued prompts drop the tag (the queue stores text only) —
        // the daemon helper only exercises the synchronous-dispatch
        // path.
        if let Some(c) = self.conversations.get_mut(&self.default_conversation_id) {
            c.next_ctx_id = prompt.ctx_id.clone();
        }
        let submission = self.submit_user_prompt(prompt.session_id.clone(), prompt.text.clone())?;
        if matches!(submission, PromptSubmission::Queued) {
            self.publish_event(
                None,
                Event::SessionPromptQueued(SessionPromptQueued {
                    session_id: prompt.session_id.clone(),
                    text: prompt.text.clone(),
                    target_agent_id: self
                        .target_agent_id_for_conversation(&self.default_conversation_id),
                    message_class: prompt.message_class,
                }),
            );
            if !prompt.message_class.is_internal() {
                self.interrupt_active_waits();
            }
            if self.selected_model.is_none() {
                self.emit_info(
                    "selected role has no available model — use /model to pick a role or enable a provider",
                );
            }
        }
        Ok(true)
    }

    fn handle_ui_switch_session(
        &mut self,
        client_id: &str,
        req: tau_proto::UiSwitchSession,
    ) -> Result<bool, HarnessError> {
        self.publish_event(Some(client_id), Event::UiSwitchSession(req.clone()));
        self.switch_session(req.new_session_id, req.reason)?;
        Ok(true)
    }

    fn handle_ui_new_agent(&mut self, req: tau_proto::UiNewAgent) -> Result<bool, HarnessError> {
        // `/agent new` is an invoking-UI-local selection reset plus this harness
        // request to rotate the default conversation. Do not publish or persist
        // the request: late/other UIs must not clear their local current-agent
        // selection in response.
        if req.session_id == self.current_session_id {
            self.rotate_default_agent_conversation();
        }
        Ok(true)
    }

    fn handle_ui_agent_state_request(
        &mut self,
        req: tau_proto::UiAgentStateRequest,
    ) -> Result<bool, HarnessError> {
        if req.session_id != self.current_session_id {
            self.emit_info(&format!(
                "harness is bound to session `{}`; agent state request for `{}` rejected",
                self.current_session_id.as_str(),
                req.session_id.as_str()
            ));
            return Ok(true);
        }
        if req.agent_id == "main" || !self.agent_conversations.contains_key(&req.agent_id) {
            self.emit_info(&format!("unknown agent `{}`", req.agent_id));
            return Ok(true);
        }
        let state = match req.action {
            UiAgentStateAction::Suspend => AgentState::Suspended,
            UiAgentStateAction::Resume => AgentState::Active,
        };
        self.set_agent_state(&req.agent_id, state);
        Ok(true)
    }

    fn handle_ui_tree_request(
        &mut self,
        client_id: &str,
        req: tau_proto::UiTreeRequest,
    ) -> Result<bool, HarnessError> {
        self.publish_event(Some(client_id), Event::UiTreeRequest(req.clone()));
        self.handle_tree_request(&req.session_id);
        Ok(true)
    }

    fn handle_ui_navigate_tree(
        &mut self,
        client_id: &str,
        req: tau_proto::UiNavigateTree,
    ) -> Result<bool, HarnessError> {
        // Validate the target node exists in *this* harness's bound
        // session before publishing — `apply_event` for
        // `UiNavigateTree` is also a no-op for unknown ids, but we want
        // a user-visible error message rather than a silent drop.
        if self.handle_navigate_tree(&req.session_id, req.node_id) {
            self.publish_event(Some(client_id), Event::UiNavigateTree(req));
        }
        Ok(true)
    }

    fn handle_ui_compact_request(
        &mut self,
        client_id: &str,
        req: tau_proto::UiCompactRequest,
    ) -> Result<bool, HarnessError> {
        self.publish_event(Some(client_id), Event::UiCompactRequest(req.clone()));
        self.handle_compact_request(req.session_id, req.target_agent_id.as_deref());
        Ok(true)
    }

    fn conversation_id_for_target_agent(
        &self,
        target_agent_id: Option<&str>,
    ) -> Option<ConversationId> {
        match target_agent_id {
            Some(agent_id) if agent_id != "main" => self.agent_conversations.get(agent_id).cloned(),
            _ => Some(self.default_conversation_id.clone()),
        }
    }

    fn target_agent_id_for_conversation(&self, cid: &ConversationId) -> Option<String> {
        self.conversations
            .get(cid)
            .and_then(|conv| conv.agent_id.as_ref())
            .filter(|agent_id| agent_id.as_str() != "main")
            .cloned()
    }

    fn handle_recall_queued_prompt(&mut self, req: &tau_proto::UiRecallQueuedPrompt) {
        if req.session_id != self.current_session_id {
            return;
        }
        let Some(cid) = self.conversation_id_for_target_agent(req.target_agent_id.as_deref())
        else {
            return;
        };
        let Some(prompt) = self.conversations.get_mut(&cid).and_then(|conv| {
            let index = conv
                .pending_prompts
                .iter()
                .rposition(|prompt| !prompt.is_internal())?;
            conv.pending_prompts.remove(index)
        }) else {
            return;
        };
        self.publish_event(
            None,
            Event::SessionPromptRecalled(SessionPromptRecalled {
                session_id: req.session_id.clone(),
                text: prompt.text,
                target_agent_id: self.target_agent_id_for_conversation(&cid),
            }),
        );
    }

    fn handle_cancel_prompt(&mut self, req: &UiCancelPrompt) {
        if req.session_id != self.current_session_id {
            return;
        }
        let Some(cid) = self.conversation_id_for_target_agent(req.target_agent_id.as_deref())
        else {
            return;
        };
        let Some(conv) = self.conversations.get_mut(&cid) else {
            return;
        };
        if matches!(conv.turn_state, ConversationTurnState::Idle) {
            self.emit_info("no active turn to cancel");
            return;
        }
        let prompt_id = conv.in_flight_prompt.clone();
        conv.pending_cancel = Some(PendingCancel {
            reason: "cancelled by user".to_owned(),
        });
        conv.pending_prompts.clear();

        if let Some(prompt_id) = prompt_id {
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id: req.session_id.clone(),
                    target_agent_id: self.target_agent_id_for_conversation(&cid),
                    session_prompt_id: Some(prompt_id),
                }),
            );
        }
        self.apply_pending_cancel_for_conversation(&cid);
    }

    fn apply_pending_cancel_for_conversation(&mut self, cid: &ConversationId) {
        let Some(cancel) = self
            .conversations
            .get(cid)
            .and_then(|conv| conv.pending_cancel.clone())
        else {
            return;
        };
        let Some(turn_state) = self
            .conversations
            .get(cid)
            .map(|conv| conv.turn_state.clone())
        else {
            return;
        };
        match turn_state {
            ConversationTurnState::Idle => {
                if let Some(conv) = self.conversations.get_mut(cid) {
                    conv.pending_cancel = None;
                    conv.pending_prompts.clear();
                }
            }
            ConversationTurnState::AgentThinking { .. } => {
                self.emit_info("cancelling current prompt");
            }
            ConversationTurnState::Compacting => {
                self.emit_info("cancelling current compaction");
            }
            ConversationTurnState::ToolsRunning { remaining_calls } => {
                self.cancel_remaining_tool_calls(cid, remaining_calls, &cancel.reason);
                if let Some(conv) = self.conversations.get_mut(cid) {
                    conv.pending_cancel = None;
                    conv.pending_prompts.clear();
                    conv.in_flight_prompt = None;
                    conv.turn_state = ConversationTurnState::Idle;
                }
                self.emit_info("cancelled current turn");
                self.try_advance_queue();
            }
        }
    }

    fn cancel_remaining_tool_calls(
        &mut self,
        cid: &ConversationId,
        remaining_calls: Vec<ToolCallId>,
        _reason: &str,
    ) {
        let remaining: std::collections::HashSet<ToolCallId> =
            remaining_calls.iter().cloned().collect();
        let mut to_cancel = self.tool_turn.cancel_queued_for(cid, &remaining);
        for call_id in remaining_calls {
            if to_cancel
                .iter()
                .any(|(queued_id, _, _)| queued_id == &call_id)
            {
                continue;
            }
            let Some(tool) = self.pending_tools.get(&call_id).cloned() else {
                continue;
            };
            to_cancel.push((call_id, tool.name, tool.tool_type));
        }

        let cancelled_call_ids: std::collections::HashSet<ToolCallId> = to_cancel
            .iter()
            .map(|(call_id, _, _)| call_id.clone())
            .collect();
        self.record_wait_tool_cancelled(&cancelled_call_ids);

        for (call_id, tool_name, tool_type) in to_cancel {
            self.publish_event(
                Some(HARNESS_CONNECTION_ID),
                Event::ToolCancelRequest(tau_proto::ToolCancelRequest {
                    target_call_id: call_id.clone(),
                }),
            );
            self.publish_for_conversation(
                cid,
                Event::ToolCancelled(ToolCancelled {
                    call_id: call_id.clone(),
                    tool_name,
                    tool_type,
                }),
            );
            self.tool_turn.mark_complete(&call_id);
            self.clear_tool_call_tracking(call_id.as_str());
        }
        if let Some(conv) = self.conversations.get_mut(cid) {
            conv.tools_in_flight = 0;
        }
    }

    pub(crate) fn is_running_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.pending_tools.contains_key(target_call_id)
    }

    pub(crate) fn is_running_cancellable_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.pending_tools.contains_key(target_call_id)
    }

    pub(crate) fn is_completed_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.completed_tool_calls.contains(target_call_id)
    }

    pub(crate) fn publish_tool_cancel_request(&mut self, target_call_id: ToolCallId) {
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::ToolCancelRequest(tau_proto::ToolCancelRequest { target_call_id }),
        );
    }

    pub(crate) fn cancel_start_agent_request(
        &mut self,
        query_id: &str,
        target_call_id: &ToolCallId,
        suppress_background_completion_prompt: bool,
    ) -> Result<(), String> {
        let mut source_id = None;
        let mut stopped_pending_agent_ids = Vec::new();
        self.pending_start_agent_requests.retain(|pending| {
            let is_canceled = pending.query.query_id == query_id
                || pending.query.tool_call_id.as_ref() == Some(target_call_id);
            if is_canceled {
                source_id = Some(pending.source_id.clone());
                stopped_pending_agent_ids.push(pending.agent_id.clone());
            }
            !is_canceled
        });
        self.stopped_agent_ids.extend(stopped_pending_agent_ids);

        if source_id.is_none() {
            source_id = self.conversations.iter().find_map(|(_, conv)| {
                if conv.parent_tool_call_id.as_ref() != Some(target_call_id) {
                    return None;
                }
                conv.source_connection.as_ref().map(|id| id.to_string())
            });
        }
        let Some(source_id) = source_id else {
            return Err("Tool call is not a running cancellable tool call".to_owned());
        };

        self.emit_info("tool call cancellation request");
        if suppress_background_completion_prompt {
            self.suppress_background_completion_prompt(target_call_id.clone());
        }
        self.cancel_delegate_side_conversation(target_call_id);
        let result = tau_proto::StartAgentResult {
            query_id: query_id.to_owned(),
            text: String::new(),
            error: Some("Tool call canceled".to_owned()),
        };
        if source_id == HARNESS_CONNECTION_ID {
            self.publish_event(Some(HARNESS_CONNECTION_ID), Event::StartAgentResult(result));
        } else {
            let _ = self.bus.send_to(
                source_id.as_str(),
                None,
                Frame::Event(Event::StartAgentResult(result)),
            );
        }
        Ok(())
    }

    fn cancel_delegate_side_conversation(&mut self, target_call_id: &ToolCallId) {
        let Some((cid, session_id, spid, turn_state, originator)) =
            self.conversations.iter().find_map(|(cid, conv)| {
                if conv.parent_tool_call_id.as_ref() != Some(target_call_id) {
                    return None;
                }
                Some((
                    cid.clone(),
                    conv.session_id.clone(),
                    conv.in_flight_prompt.clone(),
                    conv.turn_state.clone(),
                    conv.originator.clone(),
                ))
            })
        else {
            return;
        };

        let mut cancelled_calls = match turn_state {
            ConversationTurnState::ToolsRunning { remaining_calls } => remaining_calls,
            _ => Vec::new(),
        };
        cancelled_calls.extend(self.tool_turn.backgrounded_calls_for(&cid));
        cancelled_calls.extend(self.background_completion_call_ids_for_teardown(&cid));
        cancelled_calls.sort();
        cancelled_calls.dedup();
        self.cancel_remaining_tool_calls(&cid, cancelled_calls, "delegate cancel tool");
        if let Some(spid) = spid {
            self.canceled_prompts.insert(spid.clone());
            self.prompt_conversations.remove(&spid);
            self.publish_prompt_terminated(
                session_id.clone(),
                spid.clone(),
                SessionPromptTerminationReason::Canceled,
                originator,
            );
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id,
                    target_agent_id: self.target_agent_id_for_conversation(&cid),
                    session_prompt_id: Some(spid),
                }),
            );
        }
        self.release_start_agent_request(&cid);
        self.discard_background_completion_target_before_teardown(&cid);
        self.remove_conversation(&cid);
        self.try_advance_queue();
    }

    fn handle_disconnect(&mut self, connection_id: &str) {
        self.extension_activation_staging.remove(connection_id);
        self.remove_discovered_context(connection_id);
        self.interceptors.remove_connection(connection_id);
        self.fail_pending_intercept_for_disconnect(connection_id);
        self.maybe_complete_session_init_for_disconnect(connection_id);
        self.set_extension_state(connection_id, ExtensionState::Disconnected);

        let meta = self.bus.connection(connection_id).cloned();
        let is_extension = meta.as_ref().is_some_and(|meta| {
            meta.origin == ConnectionOrigin::Supervised || meta.origin == ConnectionOrigin::InMemory
        });
        if is_extension {
            self.unregister_connection_tools_for_disconnect(connection_id);
            self.action_registry.unregister_connection(connection_id);
        }

        self.fail_pending_action_invocations_for_connection(connection_id);
        self.fail_pending_tool_calls_for_connection(connection_id);
        self.pending_provider_prompts
            .retain(|_, provider_id| provider_id.as_str() != connection_id);
        self.client_writers
            .remove(&tau_proto::ConnectionId::from(connection_id));
        if self
            .provider_models_by_extension
            .remove(connection_id)
            .is_some()
        {
            self.refresh_provider_models_and_publish_state();
        }
        let Some(meta) = self.bus.disconnect(connection_id).or(meta) else {
            return;
        };
        if is_extension {
            self.emit_extension_exited(&meta.name);
        }
        if meta.origin == ConnectionOrigin::Supervised
            && let Err(error) = self.try_respawn_supervised_extension(connection_id)
        {
            self.emit_info(&format!(
                "failed to respawn extension {}: {error}",
                meta.name
            ));
        }
    }

    fn unregister_connection_tools_for_disconnect(&mut self, connection_id: &str) {
        let removing_tools: Vec<(ToolName, ToolName)> = self
            .registry
            .all_tool_names()
            .into_iter()
            .filter_map(|tool_name| {
                self.registry
                    .providers_for(tool_name.as_str())
                    .into_iter()
                    .find(|provider| provider.connection_id.as_str() == connection_id)
                    .map(|provider| {
                        (
                            tool_name.clone(),
                            self.tool_model_visible_name(&provider.tool).clone(),
                        )
                    })
            })
            .collect();

        let _ = self.registry.unregister_connection(connection_id);
        for (internal_name, visible_name) in removing_tools {
            if self
                .registry
                .providers_for(internal_name.as_str())
                .is_empty()
            {
                self.mark_tool_unavailable_for_notice(internal_name, visible_name);
            }
        }
    }

    fn is_provider_extension(&self, connection_id: &str) -> bool {
        self.extensions
            .get(connection_id)
            .is_some_and(|entry| entry.kind == ClientKind::Provider)
    }

    fn accepts_provider_event_from(
        &self,
        source_id: &str,
        event_name: &tau_proto::EventName,
    ) -> bool {
        match self.bus.connection(source_id) {
            Some(metadata) if metadata.kind == ClientKind::Provider => true,
            Some(metadata) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    source_id,
                    kind = ?metadata.kind,
                    "discarding provider event from non-provider connection"
                );
                false
            }
            None => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    source_id,
                    "discarding provider event from unknown connection"
                );
                false
            }
        }
    }

    fn provider_prompt_owner_matches(
        &self,
        source_id: &str,
        session_prompt_id: &SessionPromptId,
        event_name: tau_proto::EventName,
    ) -> bool {
        match self.pending_provider_prompts.get(session_prompt_id) {
            Some(expected) if expected.as_str() == source_id => true,
            Some(expected) => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    session_prompt_id = %session_prompt_id,
                    expected_provider = %expected,
                    source_id,
                    "discarding provider event from non-owning provider"
                );
                false
            }
            None => {
                tracing::warn!(
                    target: "tau_harness",
                    event = %event_name,
                    session_prompt_id = %session_prompt_id,
                    source_id,
                    "discarding provider event for prompt without a pending provider route"
                );
                false
            }
        }
    }

    fn fail_pending_tool_calls_for_connection(&mut self, connection_id: &str) {
        let mut failed_call_ids: Vec<ToolCallId> = self
            .pending_tool_providers
            .iter()
            .filter_map(|(call_id, provider_id)| {
                if provider_id.as_str() == connection_id {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();
        // Keep disconnect cleanup deterministic; queued work is drained only
        // after the whole sorted batch is terminalized below.
        failed_call_ids.sort();

        let mut completed_foreground_calls: Vec<(ToolCallId, ConversationId)> = Vec::new();

        for call_id in failed_call_ids {
            let Some(tool) = self.pending_tools.get(&call_id).cloned() else {
                continue;
            };
            let mut error = ToolError {
                call_id: call_id.clone(),
                tool_name: tool.name,
                tool_type: tool.tool_type,
                message: extension_disconnected_tool_call_error_message(&call_id),
                details: None,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            };
            if self.tool_turn.is_backgrounded(&call_id) {
                error.message = extension_disconnected_background_tool_call_error_message(&call_id);
                if self.tool_conversations.contains_key(call_id.as_str()) {
                    self.handle_background_tool_error_without_advancing(
                        Some(HARNESS_CONNECTION_ID),
                        error,
                    );
                } else {
                    self.publish_terminal_tool_error(None, Some(HARNESS_CONNECTION_ID), error);
                    self.tool_turn.mark_complete(&call_id);
                    self.clear_tool_call_tracking(call_id.as_str());
                }
                continue;
            }

            // Publish on the owning conversation's branch so the
            // synthesized failure folds onto the right node. Without
            // the snap, sibling side conversations could leave
            // `tree.head` on the wrong branch and the fold would land
            // there instead. Complete the failed in-flight calls without
            // draining queued calls or advancing prompts yet; disconnect
            // handling unregisters the dead provider first, then drains
            // the scheduler and completes turns after all interrupted calls
            // have been terminalized.
            let owner = self.tool_conversations.get(call_id.as_str()).cloned();
            if let Some(cid) = owner.as_ref() {
                self.publish_terminal_tool_error(Some(cid), Some(HARNESS_CONNECTION_ID), error);
            } else {
                // No conversation attribution — fall back to the
                // unsnapped publish so the error still reaches the
                // bus / log.
                self.publish_terminal_tool_error(None, Some(HARNESS_CONNECTION_ID), error);
            }
            if let Some(cid) = self.finish_tool_call_runtime_state(call_id.as_str()) {
                completed_foreground_calls.push((call_id.clone(), cid));
            }
            self.clear_tool_call_tracking(call_id.as_str());
        }

        self.drain_pending_tool_invocations_or_report();
        for (call_id, cid) in completed_foreground_calls {
            self.maybe_complete_agent_turn_for(&cid, call_id.as_str());
        }
        self.try_advance_queue();
    }

    fn send_action_error_to_client(
        &mut self,
        client_id: &str,
        invocation_id: ActionInvocationId,
        action_id: String,
        message: String,
    ) {
        let _ = self.bus.send_to(
            client_id,
            Some(HARNESS_CONNECTION_ID),
            Frame::Event(Event::ActionError(ActionError {
                invocation_id,
                action_id,
                message,
                details: None,
            })),
        );
    }

    fn handle_action_invoke(
        &mut self,
        client_id: &str,
        invoke: ActionInvoke,
    ) -> Result<bool, HarnessError> {
        if self
            .bus
            .connection(client_id)
            .is_none_or(|metadata| metadata.kind != ClientKind::Ui)
        {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                "only UI clients may invoke extension actions".to_owned(),
            );
            return Ok(true);
        }
        if invoke.session_id != self.current_session_id {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                format!(
                    "action invocation targets session `{}` but current session is `{}`",
                    invoke.session_id, self.current_session_id
                ),
            );
            return Ok(true);
        }
        if self
            .pending_action_invocations
            .contains_key(&invoke.invocation_id)
        {
            self.send_action_error_to_client(
                client_id,
                invoke.invocation_id,
                invoke.action_id,
                "duplicate pending action invocation id".to_owned(),
            );
            return Ok(true);
        }

        let provider_connection_id = match self.action_registry.route_action_invoke(&invoke) {
            Ok(provider_connection_id) => provider_connection_id,
            Err(error) => {
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    error.to_string(),
                );
                return Ok(true);
            }
        };

        match self.bus.send_to(
            provider_connection_id.as_str(),
            Some(client_id),
            Frame::Event(Event::ActionInvoke(invoke.clone())),
        ) {
            Ok(report) if !report.delivered_to.is_empty() => {
                self.pending_action_invocations.insert(
                    invoke.invocation_id.clone(),
                    PendingActionInvocation {
                        provider_connection_id,
                        requester_client_id: client_id.into(),
                        action_id: invoke.action_id,
                    },
                );
            }
            Ok(report) => {
                tracing::warn!(
                    target: "tau_harness",
                    invocation_id = %invoke.invocation_id,
                    ?report,
                    "action invocation route did not deliver"
                );
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    "action provider is unavailable".to_owned(),
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "tau_harness",
                    invocation_id = %invoke.invocation_id,
                    %error,
                    "action invocation route failed"
                );
                self.send_action_error_to_client(
                    client_id,
                    invoke.invocation_id,
                    invoke.action_id,
                    "action provider is unavailable".to_owned(),
                );
            }
        }
        Ok(true)
    }

    fn handle_action_result(&mut self, source_id: &str, result: ActionResult) {
        let Some(pending) = self
            .pending_action_invocations
            .get(&result.invocation_id)
            .cloned()
        else {
            return;
        };
        if pending.provider_connection_id.as_str() != source_id
            || pending.action_id != result.action_id
        {
            tracing::warn!(
                target: "tau_harness",
                invocation_id = %result.invocation_id,
                source_id,
                expected_provider = %pending.provider_connection_id,
                expected_action = %pending.action_id,
                action_id = %result.action_id,
                "discarding action result from non-owning or mismatched source"
            );
            return;
        }
        self.pending_action_invocations
            .remove(&result.invocation_id);
        let _ = self.bus.send_to(
            pending.requester_client_id.as_str(),
            Some(source_id),
            Frame::Event(Event::ActionResult(result)),
        );
    }

    fn handle_action_error(&mut self, source_id: &str, error: ActionError) {
        let Some(pending) = self
            .pending_action_invocations
            .get(&error.invocation_id)
            .cloned()
        else {
            return;
        };
        if pending.provider_connection_id.as_str() != source_id
            || pending.action_id != error.action_id
        {
            tracing::warn!(
                target: "tau_harness",
                invocation_id = %error.invocation_id,
                source_id,
                expected_provider = %pending.provider_connection_id,
                expected_action = %pending.action_id,
                action_id = %error.action_id,
                "discarding action error from non-owning or mismatched source"
            );
            return;
        }
        self.pending_action_invocations.remove(&error.invocation_id);
        let _ = self.bus.send_to(
            pending.requester_client_id.as_str(),
            Some(source_id),
            Frame::Event(Event::ActionError(error)),
        );
    }

    fn fail_pending_action_invocations_for_connection(&mut self, connection_id: &str) {
        let mut failed: Vec<_> = self
            .pending_action_invocations
            .iter()
            .filter_map(|(invocation_id, pending)| {
                (pending.provider_connection_id.as_str() == connection_id)
                    .then_some((invocation_id.clone(), pending.clone()))
            })
            .collect();
        failed.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        for (invocation_id, pending) in failed {
            self.pending_action_invocations.remove(&invocation_id);
            if pending.requester_client_id.as_str() == connection_id {
                continue;
            }
            self.send_action_error_to_client(
                pending.requester_client_id.as_str(),
                invocation_id,
                pending.action_id.clone(),
                format!(
                    "action `{}` was interrupted because extension disconnected",
                    pending.action_id
                ),
            );
        }
        self.pending_action_invocations
            .retain(|_, pending| pending.requester_client_id.as_str() != connection_id);
    }

    fn try_respawn_supervised_extension(
        &mut self,
        connection_id: &str,
    ) -> Result<(), HarnessError> {
        let Some(entry) = self.extensions.get_mut(connection_id) else {
            return Ok(());
        };
        let Some(config) = entry.supervised_config.clone() else {
            return Ok(());
        };
        if entry.kind == ClientKind::Provider {
            return Ok(());
        }

        entry.restart_attempt += 1;
        let attempt = entry.restart_attempt;
        let instance_id = entry.instance_id;
        let name = entry.name.clone();
        let kind = entry.kind.clone();
        let secrets = entry.secrets.clone();
        self.publish_event(
            Some("harness"),
            Event::ExtensionRestarting(tau_proto::ExtensionRestarting {
                instance_id,
                extension_name: name.clone().into(),
                pid: None,
                attempt,
                reason: Some("unexpected disconnect".to_owned()),
            }),
        );

        let log_path = extension_stderr_log_path(
            &self.sessions_dir(),
            self.current_session_id.as_str(),
            &config.name,
        )
        .map_err(|error| HarnessError::Participant(error.to_string()))?;
        tracing::info!(
            target: "tau_harness::startup",
            extension = %config.name,
            command = %config.command,
            args = ?config.args,
            attempt,
            "respawning extension",
        );
        let spawned = spawn_supervised(&config, kind.clone(), Some(log_path), &self.tx)?;
        let new_connection_id = spawned.connection_id.clone();
        tracing::info!(
            target: "tau_harness::startup",
            extension = %config.name,
            pid = spawned.child_pid,
            attempt,
            "extension respawned",
        );

        let old_key = tau_proto::ConnectionId::from(connection_id);
        self.queue_extension_connect(ExtensionConnectCommand {
            entry: ExtensionEntry {
                name,
                instance_id,
                connection_id: new_connection_id,
                kind,
                pid: Some(spawned.child_pid),
                in_process_thread: None,
                supervised_config: Some(config),
                secrets,
                restart_attempt: attempt,
                state: ExtensionState::Spawning,
                last_acked: tau_proto::LogEventId::default(),
            },
            origin: ConnectionOrigin::Supervised,
            writer_tx: spawned.writer_tx,
            initialized_ack: spawned.initialized_ack,
            replaces: Some(old_key),
        })?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Tool-call session bookkeeping
    // -----------------------------------------------------------------------
    //
    // Persistence of tool activity into the session tree is handled
    // automatically by the publish path: every published `ToolRequest`
    // / `ToolResult` / `ToolError` flows through
    // `persist_session_event`, which writes the event to the durable
    // per-session log and applies the same event to the in-memory
    // tree. The helpers below only maintain the runtime maps that
    // `session_id_for_event` reads to attribute incoming results back
    // to the originating session.

    /// Records that an extension-originated `ToolRequest` belongs to
    /// the harness's *default* conversation (the user's UI thread).
    /// Extensions don't currently carry an owning conversation on
    /// their tool tool requests; future work could extend the protocol so
    /// extension-side tools also attribute to a specific conversation.
    /// Must run *before* the request is published, so
    /// `session_id_for_event` can attribute the corresponding
    /// persisted event.
    fn track_tool_request_session(&mut self, request: &ToolRequest) {
        self.tool_conversations.insert(
            request.call_id.clone(),
            self.default_conversation_id.clone(),
        );
        self.pending_tools.insert(
            request.call_id.clone(),
            PendingTool {
                name: request.tool_name.clone(),
                internal_name: request.tool_name.clone(),
                tool_type: request.tool_type,
            },
        );
    }

    /// Releases the conversation/name/provider mappings for a
    /// completed tool call. Must run *after* the result/error event
    /// has been published, otherwise `session_id_for_event` would no
    /// longer be able to attribute the durable record.
    pub(crate) fn clear_tool_call_tracking(&mut self, call_id: &str) {
        self.completed_tool_calls.insert(call_id.into());
        self.tool_conversations.remove(call_id);
        self.pending_tools.remove(call_id);
        self.pending_tool_providers.remove(call_id);
    }

    fn validate_tool_event_source(&self, call_id: &ToolCallId, source_id: &str) -> bool {
        match self.pending_tool_providers.get(call_id) {
            Some(provider_id) => provider_id.as_str() == source_id,
            None if self.is_harness_owned_tool_call(call_id) => source_id == HARNESS_CONNECTION_ID,
            None => true,
        }
    }

    fn requires_tool_event_intake(event: &Event) -> bool {
        matches!(
            event,
            Event::ToolResult(_)
                | Event::ToolError(_)
                | Event::ProviderToolResult(_)
                | Event::ProviderToolError(_)
                | Event::ToolProgress(_)
                | Event::ToolCancelled(_)
                | Event::ToolBackgroundResult(_)
                | Event::ToolBackgroundError(_)
        )
    }

    fn is_harness_owned_tool_call(&self, call_id: &ToolCallId) -> bool {
        self.tool_conversations.contains_key(call_id)
            && self.pending_tools.get(call_id).is_some_and(|tool| {
                self.internal_tool_handlers
                    .iter()
                    .any(|handler| handler.handles(&tool.internal_name))
            })
    }

    // -----------------------------------------------------------------------
    // Lifecycle helpers
    // -----------------------------------------------------------------------

    fn find_extension_by_name(&self, name: &str) -> Option<&ExtensionEntry> {
        self.extensions.values().find(|e| e.name == name)
    }

    fn find_extension_by_connection(&self, connection_id: &str) -> Option<&ExtensionEntry> {
        self.extensions.get(connection_id)
    }

    fn publish_lifecycle_event(&mut self, event: Event) {
        let transient = event.defaults_to_transient();
        self.commit_event(Some("harness"), event, transient, None);
    }

    fn emit_extension_starting(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} starting"));
        self.publish_lifecycle_event(Event::ExtensionStarting(tau_proto::ExtensionStarting {
            instance_id: iid,
            extension_name: extension_name.into(),
            pid,
        }));
    }

    fn emit_extension_ready(&mut self, connection_id: &str) {
        let Some(ext) = self.find_extension_by_connection(connection_id) else {
            return;
        };
        let name = ext.name.clone();
        let iid = ext.instance_id;
        let pid = ext.pid;
        self.lifecycle_messages
            .push(format!("extension {name} ready"));
        self.publish_lifecycle_event(Event::ExtensionReady(tau_proto::ExtensionReady {
            instance_id: iid,
            extension_name: name.into(),
            pid,
        }));
    }

    fn emit_extension_exited(&mut self, extension_name: &str) {
        let (iid, pid) = self
            .find_extension_by_name(extension_name)
            .map(|e| (e.instance_id, e.pid))
            .unwrap_or((0.into(), None));
        self.lifecycle_messages
            .push(format!("extension {extension_name} exited"));
        self.publish_lifecycle_event(Event::ExtensionExited(tau_proto::ExtensionExited {
            instance_id: iid,
            extension_name: extension_name.into(),
            pid,
            exit_code: None,
            signal: None,
        }));
    }

    fn check_config_exists(&mut self) {
        if let Some(dir) = tau_config::settings::config_dir()
            && !dir.join("harness.yaml").exists()
        {
            self.emit_info_important(
                "no config found; run `tau init` to create sample config files",
            );
        }
    }

    /// Surface settings-file parse errors captured during the initial
    /// load as `Important` `HarnessInfo`. The loaders already fell
    /// back to defaults and wrote a short stderr line, but stderr is
    /// hidden once the TUI takes over the terminal — without this the
    /// user's only symptom is "my extensions vanished" / "my roles changed"
    /// with no clue why.
    ///
    /// Taking the error as a parameter (instead of re-parsing the file
    /// here) keeps startup to a single parse and avoids a race where the
    /// user fixes the file between the two reads.
    ///
    /// `cli.json5` is intentionally not handled here: the CLI fails
    /// fast on a malformed `cli.json5` before the harness ever
    /// spawns, so there's no "silently fell back to defaults" case
    /// to surface.
    fn emit_startup_settings_errors(
        &mut self,
        harness_settings_error: Option<tau_config::settings::SettingsError>,
    ) {
        if let Some(error) = harness_settings_error {
            self.emit_info_important(&format!("harness.yaml failed to parse — ignored.\n{error}"));
        }
    }

    fn emit_missing_default_role(&mut self, missing: Option<MissingDefaultRole>) {
        if let Some(MissingDefaultRole {
            requested,
            fallback,
        }) = missing
        {
            self.emit_info_important(&format!(
                "defaultRole `{requested}` is not configured; selected `{fallback}` instead"
            ));
        }
    }

    /// Push the configured `config` value (from `harness.yaml`) to
    /// the just-said-Hello extension. Sends point-to-point so it
    /// arrives even if the extension hasn't subscribed to the
    /// `lifecycle` category yet. In-process extensions don't carry
    /// a `supervised_config` so they get the empty default — they
    /// already accept configuration via constructor parameters.
    fn send_lifecycle_configure(&mut self, source_id: &str) {
        let Some(entry) = self.extensions.get(source_id) else {
            return;
        };
        let config_json = entry
            .supervised_config
            .as_ref()
            .map(|cfg| cfg.config.clone())
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        let secrets = entry.secrets.clone();
        let state_dir =
            match tau_config::settings::extension_state_dir_of(&self.state_dir, &entry.name) {
                Ok(state_dir) => state_dir,
                Err(error) => {
                    tracing::warn!(
                        extension = %entry.name,
                        error = %error,
                        "refusing to configure extension with unsafe state directory name"
                    );
                    let _ = self.bus.send_to(
                        source_id,
                        None,
                        Frame::Message(Message::ConfigError(tau_proto::ConfigError {
                            message: error.to_string(),
                        })),
                    );
                    return;
                }
            };
        if let Err(error) = std::fs::create_dir_all(&state_dir) {
            tracing::warn!(
                extension = %entry.name,
                state_dir = %state_dir.display(),
                error = %error,
                "failed to create extension state directory before configure"
            );
        }
        let _ = self.bus.send_to(
            source_id,
            None,
            Frame::Message(Message::Configure(tau_proto::Configure {
                config: tau_proto::json_to_cbor(&config_json),
                state_dir: Some(state_dir),
                secrets,
            })),
        );
    }

    pub(crate) fn emit_info(&mut self, message: &str) {
        self.emit_info_with_level(message, tau_proto::HarnessInfoLevel::Normal);
    }

    pub(crate) fn emit_info_important(&mut self, message: &str) {
        self.emit_info_with_level(message, tau_proto::HarnessInfoLevel::Important);
    }

    fn emit_info_with_level(&mut self, message: &str, level: tau_proto::HarnessInfoLevel) {
        self.publish_event(
            Some("harness"),
            Event::HarnessInfo(tau_proto::HarnessInfo {
                message: message.to_owned(),
                level,
            }),
        );
    }

    fn publish_prompt_terminated(
        &mut self,
        session_id: SessionId,
        session_prompt_id: SessionPromptId,
        reason: SessionPromptTerminationReason,
        originator: PromptOriginator,
    ) {
        self.publish_event(
            None,
            Event::SessionPromptTerminated(SessionPromptTerminated {
                session_id,
                session_prompt_id,
                reason,
                originator,
            }),
        );
    }

    fn remove_discovered_context(&mut self, source_id: &str) {
        self.discovered_skills.retain(|_, skill| {
            matches!(skill.source, DiscoveredSkillSource::BuiltIn { .. })
                || skill.source_id != source_id
        });
        self.discovered_agents_files
            .retain(|file| file.source_id != source_id);
    }

    fn record_discovered_skill(&mut self, source_id: &str, skill: &tau_proto::ExtSkillAvailable) {
        if let Some(message) = tau_skills::skill_name_validation_message(skill.name.as_str()) {
            self.emit_info_important(&format!(
                "skill skipped: {} from {} has invalid name: {}",
                skill.name,
                skill.file_path.display(),
                message,
            ));
            return;
        }

        let description = if tau_skills::MAX_DESCRIPTION_LENGTH < skill.description.len() {
            self.emit_info_important(&format!(
                "skill warning: {} from {} description exceeds {} bytes ({}); truncating",
                skill.name,
                skill.file_path.display(),
                tau_skills::MAX_DESCRIPTION_LENGTH,
                skill.description.len(),
            ));
            tau_skills::truncate_description(&skill.description).into_owned()
        } else {
            skill.description.clone()
        };

        let collision = self
            .discovered_skills
            .get(&skill.name)
            .filter(|existing| existing.source_id != source_id)
            .map(|existing| (existing.source_id.clone(), existing.source.label()));

        if let Some((existing_source, existing_label)) = collision {
            self.emit_info_important(&format!(
                "skill collision: {} from {} ignored; keeping {} from {}",
                skill.name,
                skill.file_path.display(),
                existing_label,
                existing_source,
            ));
            return;
        }

        self.discovered_skills.insert(
            skill.name.clone(),
            DiscoveredSkill {
                source_id: source_id.into(),
                description,
                source: DiscoveredSkillSource::File(std::path::PathBuf::from(&skill.file_path)),
                add_to_prompt: skill.add_to_prompt,
            },
        );
    }

    fn session_init_provider_ids(&self) -> std::collections::HashSet<tau_proto::ConnectionId> {
        let event = Event::SessionStarted(tau_proto::SessionStarted {
            session_id: "probe".into(),
            reason: tau_proto::SessionStartReason::Initial,
        });
        self.bus
            .connections()
            .into_iter()
            .filter(|connection| {
                connection.kind == ClientKind::Tool
                    && connection.origin != ConnectionOrigin::Socket
                    && self
                        .bus
                        .subscriptions(connection.id.as_str())
                        .is_some_and(|selectors| selector_matches_event(selectors, &event))
            })
            .map(|connection| connection.id)
            .collect()
    }

    pub(crate) fn session_initialized(&self, session_id: &SessionId) -> bool {
        self.initialized_sessions.contains(session_id)
    }

    fn available_delegate_role_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .available_roles
            .keys()
            .filter(|name| {
                model_for_role(&self.provider_model_info, &self.available_roles, name).is_some()
            })
            .cloned()
            .collect();
        names.sort();
        names
    }

    fn available_delegate_roles_message(&self) -> String {
        let roles = self.available_delegate_role_names();
        if roles.is_empty() {
            "available roles: (none)".to_owned()
        } else {
            format!("available roles: {}", roles.join(", "))
        }
    }

    fn resolve_start_agent_request_role(
        &self,
        query: &tau_proto::StartAgentRequest,
    ) -> Result<String, String> {
        let requested = if let Some(role) = query.role.as_deref() {
            role
        } else if query.tool_call_id.is_some() {
            "senior-engineer"
        } else {
            self.selected_role.as_str()
        };

        if self.available_roles.contains_key(requested)
            && model_for_role(&self.provider_model_info, &self.available_roles, requested).is_some()
        {
            return Ok(requested.to_owned());
        }

        let reason = if query.role.is_none() && query.tool_call_id.is_some() {
            "delegate requires default role `senior-engineer`, but it is not available"
        } else if self.available_roles.contains_key(requested) {
            "requested role is not backed by an available model"
        } else {
            "requested role does not exist"
        };
        Err(format!(
            "{reason}: `{requested}`; {}",
            self.available_delegate_roles_message()
        ))
    }

    fn fail_start_agent_request(&mut self, source_id: &str, query_id: String, error: String) {
        let result = tau_proto::StartAgentResult {
            query_id,
            text: String::new(),
            error: Some(error),
        };
        if source_id == HARNESS_CONNECTION_ID {
            self.publish_event(Some(HARNESS_CONNECTION_ID), Event::StartAgentResult(result));
        } else {
            let _ = self.bus.send_to(
                source_id,
                None,
                Frame::Event(Event::StartAgentResult(result)),
            );
        }
    }

    /// Queue an extension-started sub-agent request onto the harness-owned
    /// global execution-mode scheduler.
    ///
    /// Normal tool calls still use the per-conversation scheduler in
    /// `drain_pending_tool_invocations`. This queue is only for
    /// `StartAgentRequest` side conversations, so delegate, notifications, and
    /// future extension-owned sub-agents all share one global lane.
    fn handle_start_agent_request(
        &mut self,
        source_id: &str,
        query: tau_proto::StartAgentRequest,
    ) -> Result<(), HarnessError> {
        let query_id = query.query_id.clone();
        let pending = match self.prepare_start_agent_request(source_id, query) {
            Ok(Some(pending)) => pending,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.fail_start_agent_request(source_id, query_id, error);
                return Ok(());
            }
        };
        let accepted = tau_proto::StartAgentAccepted {
            query_id: pending.query.query_id.clone(),
            agent_id: pending.agent_id.clone(),
        };
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::StartAgentAccepted(accepted.clone()),
        );
        let _ = self.bus.send_to(
            source_id,
            None,
            Frame::Event(Event::StartAgentAccepted(accepted)),
        );
        self.pending_start_agent_requests.push_back(pending);
        self.drain_pending_start_agent_requests()
    }

    /// Enqueue an internal start-agent request and return its minted agent id.
    pub(crate) fn enqueue_internal_start_agent_request_without_draining(
        &mut self,
        query: tau_proto::StartAgentRequest,
    ) -> Result<String, String> {
        let Some(pending) = self.prepare_start_agent_request(HARNESS_CONNECTION_ID, query)? else {
            return Err("duplicate delegate start-agent request".to_owned());
        };
        let agent_id = pending.agent_id.clone();
        let accepted = tau_proto::StartAgentAccepted {
            query_id: pending.query.query_id.clone(),
            agent_id: agent_id.clone(),
        };
        self.publish_event(
            Some(HARNESS_CONNECTION_ID),
            Event::StartAgentAccepted(accepted.clone()),
        );
        let _ = self.bus.send_to(
            HARNESS_CONNECTION_ID,
            None,
            Frame::Event(Event::StartAgentAccepted(accepted)),
        );
        self.pending_start_agent_requests.push_back(pending);
        Ok(agent_id)
    }

    fn prepare_start_agent_request(
        &mut self,
        source_id: &str,
        query: tau_proto::StartAgentRequest,
    ) -> Result<Option<PendingStartAgentRequest>, String> {
        let extension_name = self
            .extensions
            .get(source_id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| source_id.to_owned());
        let role = self.resolve_start_agent_request_role(&query)?;
        let cid = ConversationId::new(format!("start-agent-{}-{}", extension_name, query.query_id));
        if self.conversations.contains_key(&cid)
            || self
                .pending_start_agent_requests
                .iter()
                .any(|pending| pending.cid == cid)
        {
            self.emit_info(&format!(
                "ignoring duplicate start-agent-request `{}` from `{}` — already in flight",
                query.query_id, extension_name
            ));
            return Ok(None);
        }

        // Resolve the parent conversation at enqueue time: tool-backed requests
        // inherit from the conversation that owns the triggering tool call;
        // non-tool tool requests inherit from the default user conversation.
        let parent_cid = query
            .tool_call_id
            .as_ref()
            .and_then(|call_id| self.tool_conversations.get(call_id))
            .cloned()
            .unwrap_or_else(|| self.default_conversation_id.clone());
        let agent_id = query.agent_id.clone();
        if agent_id.trim().is_empty() {
            return Err("start-agent request agent_id must not be empty".to_owned());
        }
        if self.agent_conversations.contains_key(&agent_id)
            || self
                .pending_start_agent_requests
                .iter()
                .any(|pending| pending.agent_id == agent_id)
        {
            return Err(format!("start-agent agent_id collision: `{agent_id}`"));
        }

        Ok(Some(PendingStartAgentRequest {
            source_id: source_id.to_owned(),
            extension_name,
            query,
            role,
            cid,
            parent_cid,
            agent_id,
            pending_agent_messages: VecDeque::new(),
        }))
    }

    /// Dispatch queued `StartAgentRequest`s while the global sub-agent
    /// scheduler allows them through.
    ///
    /// Rules:
    /// - Shared may start when no incompatible active Exclusive sub-agent
    ///   exists.
    /// - Update may start when no incompatible active Update or Exclusive
    ///   sub-agent exists.
    /// - Exclusive may start when no incompatible active sub-agent exists.
    /// - FIFO is preserved for independent work: once an Update or Exclusive
    ///   reaches the front and is blocked, later independent compatible
    ///   requests do not jump it.
    /// - Reentrant descendants of an active Exclusive are allowed to pass as
    ///   part of that exclusive subtree. Without this exception, an exclusive
    ///   delegate that asks its own sub-agent to delegate would deadlock behind
    ///   itself. Update does not get this exception because it is only meant to
    ///   share with Shared work. Descendancy is computed from the side
    ///   conversation's stored parent conversation, with the older
    ///   `parent_tool_call_id` mapping as a fallback for manually seeded tests.
    pub(crate) fn drain_pending_start_agent_requests(&mut self) -> Result<(), HarnessError> {
        loop {
            let Some(idx) = self.next_dispatchable_start_agent_request_index() else {
                return Ok(());
            };
            let pending = self
                .pending_start_agent_requests
                .remove(idx)
                .expect("index just located");
            self.start_agent_request(pending)?;
        }
    }

    fn next_dispatchable_start_agent_request_index(&self) -> Option<usize> {
        let mut blocked_barrier_ahead = false;
        for (idx, pending) in self.pending_start_agent_requests.iter().enumerate() {
            let can_start = self.start_agent_request_can_start(pending);
            if !can_start {
                if pending.query.execution_mode.blocks_fifo_when_waiting() {
                    blocked_barrier_ahead = true;
                }
                continue;
            }
            if blocked_barrier_ahead && !self.query_belongs_to_active_exclusive(&pending.parent_cid)
            {
                continue;
            }
            return Some(idx);
        }
        None
    }

    fn start_agent_request_can_start(&self, pending: &PendingStartAgentRequest) -> bool {
        self.active_start_agent_requests
            .iter()
            .all(|(active_cid, active)| {
                !self.active_start_agent_request_is_incompatible(
                    active_cid,
                    active,
                    &pending.parent_cid,
                    pending.query.execution_mode,
                )
            })
    }

    fn active_start_agent_request_is_incompatible(
        &self,
        active_cid: &ConversationId,
        active: &ActiveStartAgentRequest,
        candidate_parent_cid: &ConversationId,
        candidate_mode: tau_proto::ToolExecutionMode,
    ) -> bool {
        if let Some(exclusive_root) = self.active_exclusive_ancestor_for(candidate_parent_cid)
            && self.conversation_descends_from(active_cid, &exclusive_root)
        {
            return false;
        }
        !candidate_mode.can_overlap_with(active.execution_mode)
    }

    fn query_belongs_to_active_exclusive(&self, parent_cid: &ConversationId) -> bool {
        self.active_exclusive_ancestor_for(parent_cid).is_some()
    }

    fn active_exclusive_ancestor_for(&self, cid: &ConversationId) -> Option<ConversationId> {
        self.active_start_agent_requests
            .iter()
            .find_map(|(active_cid, active)| {
                matches!(
                    active.execution_mode,
                    tau_proto::ToolExecutionMode::Exclusive
                )
                .then_some(active_cid)
                .filter(|active_cid| self.conversation_descends_from(cid, active_cid))
                .cloned()
            })
    }

    fn conversation_descends_from(&self, cid: &ConversationId, ancestor: &ConversationId) -> bool {
        let mut current = Some(cid.clone());
        let mut seen = std::collections::HashSet::new();
        while let Some(current_cid) = current {
            if &current_cid == ancestor {
                return true;
            }
            if !seen.insert(current_cid.clone()) {
                return false;
            }
            current = self.conversations.get(&current_cid).and_then(|conv| {
                conv.parent_conversation_id
                    .as_ref()
                    .filter(|parent_cid| self.conversations.contains_key(*parent_cid))
                    .cloned()
                    .or_else(|| {
                        conv.parent_tool_call_id
                            .as_ref()
                            .and_then(|call_id| self.tool_conversations.get(call_id))
                            .cloned()
                    })
            });
        }
        false
    }

    /// Spawn a fresh side conversation for an extension's
    /// [`tau_proto::StartAgentRequest`] and dispatch it after the global
    /// scheduler admits it.
    ///
    /// Two forking modes depending on whether the request is tool-backed:
    ///
    /// - **Tool-backed (`tool_call_id: Some(...)`, e.g. `delegate`)**: the
    ///   sub-agent starts with a *fresh* context — only the delegated
    ///   instruction, no inherited messages from the parent (no user framing,
    ///   no completed prior turns, no in-flight tool blocks). The parent agent
    ///   is responsible for putting everything the sub-agent needs into the
    ///   `prompt`. This applies uniformly at any nesting depth so deeper
    ///   sub-agents can't see (and restage) ancestor task framing.
    ///
    /// - **Non-tool (`tool_call_id: None`, e.g. notifications' idle summary)**:
    ///   the side conv inherits the parent conversation's current head (and
    ///   `chain_anchor`, if any) so the assembled prompt actually contains the
    ///   user's recent history. The whole point of this flow is to summarize
    ///   what the user/agent were doing — that needs the conversation it is
    ///   summarizing. Sharing the prefix also lets prompt caching reuse the
    ///   parent's cached transcript verbatim, since the only delta is the
    ///   appended instruction.
    fn start_agent_request(
        &mut self,
        pending: PendingStartAgentRequest,
    ) -> Result<(), HarnessError> {
        let PendingStartAgentRequest {
            source_id,
            extension_name,
            query,
            role,
            cid,
            parent_cid,
            agent_id,
            pending_agent_messages,
        } = pending;
        let parent_call_id = query.tool_call_id.clone();
        let task_name = query.task_name.clone();
        let conversation_role = if query.tool_call_id.is_some() || query.role.is_some() {
            Some(role)
        } else {
            None
        };
        let execution_mode = query.execution_mode;
        let delegate_execution_mode = parent_call_id.as_ref().map(|_| execution_mode);
        let parent_conv = self
            .conversations
            .get(&parent_cid)
            .expect("parent conversation always present");
        let session_id = parent_conv.session_id.clone();
        // Tool-backed: fresh fork (head=None). Non-tool: inherit the
        // parent's branch + stateful-chain anchor.
        let (initial_head, initial_chain_anchor) = if query.tool_call_id.is_some() {
            (None, None)
        } else {
            (parent_conv.head, parent_conv.chain_anchor.clone())
        };

        let originator = tau_proto::PromptOriginator::Extension {
            name: extension_name.clone().into(),
            query_id: query.query_id.clone(),
        };
        let mut conv = Conversation::new(
            cid.clone(),
            session_id.clone(),
            originator,
            initial_head,
            Some(source_id.into()),
        );
        // For tool-backed extensions (currently just `delegate`)
        // record the parent call id and task name so subsequent
        // sub-agent state changes can be surfaced to the user under
        // that tool block via `DelegateProgress`.
        conv.parent_tool_call_id = parent_call_id;
        conv.parent_conversation_id = Some(parent_cid.clone());
        conv.task_name = task_name;
        conv.delegate_input_stats = query.input_stats;
        conv.role = conversation_role;
        conv.agent_id = Some(agent_id.clone());
        conv.delegate_execution_mode = delegate_execution_mode;
        conv.chain_anchor = initial_chain_anchor;
        conv.pending_prompts = pending_agent_messages;
        self.agent_conversations
            .insert(agent_id.clone(), cid.clone());
        self.conversations.insert(cid.clone(), conv);
        self.active_start_agent_requests
            .insert(cid.clone(), ActiveStartAgentRequest { execution_mode });
        if delegate_execution_mode.is_some() {
            self.set_agent_state(&agent_id, AgentState::ActiveDelegated);
        }

        // Emit the initial progress snapshot (`%0/0`, no ctx
        // info yet) so the parent's tool block flips from `…` to the
        // structured form as soon as the side conversation exists,
        // without waiting for the sub-agent's first event.
        self.emit_delegate_progress(&cid);

        // Publish the UiPromptSubmitted on the side conversation's
        // branch and dispatch the agent. The fold parents the new
        // UserMessage at `conv.head` — root for the fresh tool-backed
        // mode, or the parent conv's tip for the inherited non-tool
        // mode. `send_prompt_to_agent_for` then reads `conv.head`
        // (which the post-commit hook just snapped to the new
        // UserMessage) and `assemble_conversation_from` walks back to
        // root, yielding either `[instruction]` alone or the full
        // inherited transcript + `instruction`.
        //
        // If an interceptor is registered on `ui.prompt_submitted`
        // (e.g. `tau-ext-test-dummy`'s tao→tau corrector) the publish
        // parks in `pending_intercept` and won't commit until the
        // interceptor replies — running `send_prompt_to_agent_for`
        // synchronously then would assemble messages from a still-`None`
        // `conv.head` and the agent would call the provider with an
        // empty `input`. Use the user-message gate so dispatch happens
        // only after this side conversation's instruction actually folds.
        self.publish_for_conversation(
            &cid,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id,
                text: query.instruction,
                target_agent_id: Some(agent_id.clone()),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::Extension {
                    name: extension_name.into(),
                    query_id: query.query_id,
                },
                ctx_id: None,
            }),
        );
        self.dispatch_prompt_after_user_message_publish(&cid);
        Ok(())
    }

    fn detach_completed_tool_backed_start_agent(&mut self, cid: &ConversationId) {
        if let Some(conv) = self.conversations.get_mut(cid) {
            // A completed delegate remains addressable by its `agent_id`, but
            // it is no longer fulfilling the parent tool call or owned by the
            // extension query that started it. Clearing the side-query fields
            // makes later user prompts behave like a normal active conversation
            // on the same branch.
            conv.originator = tau_proto::PromptOriginator::User;
            conv.source_connection = None;
            conv.parent_tool_call_id = None;
            conv.parent_conversation_id = None;
            conv.task_name = None;
            conv.delegate_execution_mode = None;
            conv.delegate_input_stats = Default::default();
        }
    }

    fn release_start_agent_request(&mut self, cid: &ConversationId) {
        if self.active_start_agent_requests.remove(cid).is_some()
            && let Err(error) = self.drain_pending_start_agent_requests()
        {
            self.emit_info(&format!("queued start-agent dispatch failed: {error}"));
        }
    }

    /// Publish a `DelegateProgress` snapshot for `cid` if it is a side
    /// conversation backing a `delegate` tool call. No-op for the
    /// default conversation and for non-tool start-agent requests.
    fn emit_delegate_progress(&mut self, cid: &ConversationId) {
        let Some(conv) = self.conversations.get(cid) else {
            return;
        };
        let (Some(call_id), Some(task_name)) =
            (conv.parent_tool_call_id.clone(), conv.task_name.clone())
        else {
            return;
        };
        let role = conv.role.clone();
        let execution_mode = conv.delegate_execution_mode;
        let ctx_window = conv.context_input_tokens.and_then(|_| {
            self.model_for_conversation_role(conv)
                .as_ref()
                .and_then(|m| context_window_for_model(&self.provider_model_info, m))
        });
        let display = build_delegate_progress_display(
            &task_name,
            conv.context_input_tokens,
            conv.context_percent_used,
            ctx_window,
            conv.tools_in_flight,
            conv.tools_total,
            conv.delegate_input_stats,
        );
        let progress = tau_proto::DelegateProgress {
            call_id,
            task_name,
            role,
            execution_mode,
            ctx_percent: conv.context_percent_used,
            ctx_input_tokens: conv.context_input_tokens,
            ctx_window,
            tools_in_flight: conv.tools_in_flight,
            tools_total: conv.tools_total,
            display: Some(display),
        };
        self.publish_event(None, Event::ToolDelegateProgress(progress));
    }

    /// Emit a `UiNavigateTree` to move the *UI's* view cursor back
    /// to the default conversation's tip after a side conversation
    /// finishes. Only a UI affordance now — fold parentage no
    /// longer depends on `tree.head()` (Phase 4 of the interception
    /// refactor: each publish carries its explicit parent), so
    /// nothing in the harness's append path needs the bounce. UIs
    /// that subscribe to `ui.navigate_tree` rely on this to render
    /// the user back on their main branch when a delegated query
    /// completes.
    fn snap_to_default_conversation(&mut self) {
        let session_id = self
            .conversations
            .get(&self.default_conversation_id)
            .map(|c| c.session_id.clone())
            .expect("default conversation always present");
        let want = self
            .conversations
            .get(&self.default_conversation_id)
            .and_then(|c| c.head);
        let have = self
            .store
            .session(session_id.as_str())
            .and_then(|t| t.head());
        if want != have
            && let Some(target) = want
        {
            self.publish_event(
                None,
                Event::UiNavigateTree(tau_proto::UiNavigateTree {
                    session_id,
                    node_id: target.get(),
                }),
            );
        }
    }

    fn maybe_start_auto_compaction_for_user_prompt(
        &mut self,
        cid: &ConversationId,
        text: &str,
    ) -> bool {
        if !self.should_auto_compact_for_conversation(cid) {
            return false;
        }
        self.start_auto_compaction_for_conversation(
            cid,
            PendingCompactionResume::UserPrompt(text.to_owned()),
        );
        true
    }

    fn handle_compact_request(&mut self, session_id: SessionId, target_agent_id: Option<&str>) {
        if session_id != self.current_session_id {
            self.emit_info(&format!(
                "cannot compact session `{session_id}` in this harness; active session is `{}`",
                self.current_session_id
            ));
            return;
        }
        let Some(cid) = self.conversation_id_for_target_agent(target_agent_id) else {
            self.emit_info("unknown agent for compaction");
            return;
        };
        if self.pending_compaction_targeted_at(&cid) {
            self.emit_info("compaction is already in progress");
            return;
        }
        if self.dispatch_blocked_for(&cid) {
            self.emit_info("cannot compact while a prompt or tool turn is in flight");
            return;
        }
        if !self.selected_model_supports_compaction() {
            self.emit_info("selected model does not support remote compaction");
            return;
        }
        let Some(conv) = self.conversations.get(&cid) else {
            self.emit_info("default conversation is missing");
            return;
        };
        let Some(tree) = self.store.session(conv.session_id.as_str()) else {
            self.emit_info("nothing to compact yet");
            return;
        };
        let prompt_context = assemble_prompt_context_from(tree, conv.head);
        if prompt_context.context_items.is_empty() {
            self.emit_info("nothing to compact yet");
            return;
        }
        self.start_auto_compaction_for_conversation(&cid, PendingCompactionResume::None);
    }

    fn maybe_start_auto_compaction_for_followup(&mut self, cid: &ConversationId) -> bool {
        if !self.should_auto_compact_for_conversation(cid) {
            return false;
        }
        self.start_auto_compaction_for_conversation(cid, PendingCompactionResume::FollowupTurn);
        true
    }

    fn should_auto_compact_for_conversation(&self, cid: &ConversationId) -> bool {
        if self.pending_compaction_targeted_at(cid) {
            return false;
        }
        if !self.selected_model_supports_compaction() {
            return false;
        }
        let current_percent = self
            .conversations
            .get(cid)
            .and_then(|conv| conv.context_percent_used)
            .or_else(|| {
                (cid == &self.default_conversation_id)
                    .then_some(self.current_session_state.context_percent_used)
                    .flatten()
            });
        let threshold = self.compaction_threshold_for_conversation(cid);
        current_percent.is_some_and(|p| threshold <= p)
    }

    fn compaction_threshold_for_conversation(&self, cid: &ConversationId) -> u8 {
        let role_name = self.role_name_for_conversation_id(cid);
        self.available_roles
            .get(&role_name)
            .and_then(|role| role.compaction_threshold)
            .unwrap_or(AUTO_COMPACTION_CONTEXT_PERCENT)
    }

    fn selected_model_supports_compaction(&self) -> bool {
        let Some(model) = self.selected_model.as_ref() else {
            return false;
        };
        self.provider_model_info
            .get(model)
            .is_some_and(|info| info.supports_compaction)
    }

    fn refresh_provider_model_info(&mut self) {
        let mut provider_model_info = HashMap::new();
        let mut provider_model_routes = HashMap::new();
        let mut source_ids: Vec<_> = self.provider_models_by_extension.keys().collect();
        source_ids.sort();
        for source_id in source_ids {
            let Some(models) = self.provider_models_by_extension.get(source_id) else {
                continue;
            };
            let connection_id = tau_proto::ConnectionId::from(source_id.as_str());
            for model in models {
                provider_model_info.insert(model.id.clone(), model.clone());
                provider_model_routes.insert(model.id.clone(), connection_id.clone());
            }
        }
        self.provider_model_info = provider_model_info;
        self.provider_model_routes = provider_model_routes;
    }

    fn refresh_available_models(&mut self) {
        self.refresh_provider_model_info();
        let mut models: Vec<ModelId> = self.provider_model_info.keys().cloned().collect();
        models.sort();
        self.available_models = models;
    }

    fn role_after_update(
        &mut self,
        role_name: &str,
        action: tau_proto::UiRoleUpdateAction,
    ) -> Option<tau_config::settings::AgentRole> {
        let mut next_role = self
            .available_roles
            .get(role_name)
            .cloned()
            .unwrap_or_default();

        match action {
            tau_proto::UiRoleUpdateAction::Delete => unreachable!("handled by caller"),
            tau_proto::UiRoleUpdateAction::SetModel { model } => {
                next_role.model = model;
            }
            tau_proto::UiRoleUpdateAction::SetEffort { effort } => {
                next_role.effort = effort;
            }
            tau_proto::UiRoleUpdateAction::SetVerbosity { verbosity } => {
                next_role.verbosity = verbosity;
            }
            tau_proto::UiRoleUpdateAction::SetThinkingSummary { thinking_summary } => {
                next_role.thinking_summary = thinking_summary;
            }
            tau_proto::UiRoleUpdateAction::SetServiceTier { service_tier } => {
                next_role.service_tier = service_tier;
            }
            tau_proto::UiRoleUpdateAction::SetCompactionThreshold {
                compaction_threshold,
            } => {
                next_role.compaction_threshold = compaction_threshold;
            }
            tau_proto::UiRoleUpdateAction::SetTools { tools } => {
                next_role.tools = tools;
            }
            tau_proto::UiRoleUpdateAction::SetEnableTools { enable_tools } => {
                next_role.enable_tools = enable_tools;
            }
            tau_proto::UiRoleUpdateAction::SetDisableTools { disable_tools } => {
                next_role.disable_tools = disable_tools;
            }
        }

        Some(next_role)
    }

    fn reconcile_selected_model_with_available(&mut self) {
        let previous_model = self.selected_model.clone();
        self.selected_model = select_model_for_role(
            &self.provider_model_info,
            &self.available_roles,
            &self.selected_role,
        );
        if previous_model != self.selected_model {
            self.current_session_state.context_input_tokens = None;
            self.current_session_state.context_cached_tokens = None;
            self.current_session_state.context_percent_used = None;
        }
    }

    fn refresh_provider_models_and_publish_state(&mut self) {
        let had_routable_model = self
            .selected_model
            .as_ref()
            .is_some_and(|model| self.provider_model_routes.contains_key(model));
        self.refresh_available_models();
        self.reconcile_selected_model_with_available();
        self.publish_available_model_state();
        let has_routable_model = self
            .selected_model
            .as_ref()
            .is_some_and(|model| self.provider_model_routes.contains_key(model));
        if !had_routable_model && has_routable_model && self.turn_state.is_idle() {
            self.try_advance_queue();
        }
    }

    fn publish_available_model_state(&mut self) {
        self.publish_event(
            None,
            Event::HarnessModelsAvailable(tau_proto::HarnessModelsAvailable {
                models: self.available_models.clone(),
            }),
        );
        self.publish_event(
            None,
            Event::HarnessRolesAvailable(tau_proto::HarnessRolesAvailable {
                roles: role_infos(
                    &self.provider_model_info,
                    &self.available_roles,
                    &self.available_models,
                ),
                groups: self.current_role_groups(),
            }),
        );
        self.publish_delegate_roles_context();
        self.publish_current_model_state();
    }

    fn current_role_groups(&self) -> Vec<tau_proto::HarnessRoleGroup> {
        let mut grouped = HashSet::new();
        let mut groups = Vec::new();
        for group in &self.available_role_groups {
            let roles: Vec<_> = group
                .roles
                .iter()
                .filter(|role| self.available_roles.contains_key(*role))
                .inspect(|role| {
                    grouped.insert((*role).clone());
                })
                .cloned()
                .collect();
            if !roles.is_empty() {
                groups.push(tau_proto::HarnessRoleGroup {
                    name: group.name.clone(),
                    roles,
                });
            }
        }
        let mut ungrouped: Vec<_> = self
            .available_roles
            .keys()
            .filter(|role| !grouped.contains(*role))
            .cloned()
            .collect();
        ungrouped.sort();
        groups.extend(
            ungrouped
                .into_iter()
                .map(|role| tau_proto::HarnessRoleGroup {
                    name: role.clone(),
                    roles: vec![role],
                }),
        );
        groups
    }

    fn publish_current_model_state(&mut self) {
        let selected_model = self.selected_model.clone();
        let (effort_levels, verbosity_levels, thinking_levels) =
            if let Some(model) = selected_model.as_ref() {
                (
                    efforts_for_model(&self.provider_model_info, model),
                    verbosities_for_model(&self.provider_model_info, model),
                    thinking_summaries_for_model(&self.provider_model_info, model),
                )
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };
        let context_window = selected_model
            .as_ref()
            .and_then(|model| context_window_for_model(&self.provider_model_info, model));
        self.current_session_state.context_percent_used = match (
            context_window,
            self.current_session_state.context_input_tokens,
        ) {
            (Some(context_window), Some(input_tokens)) => {
                Some(context_percent_used(input_tokens, context_window))
            }
            _ => None,
        };
        let (live_settings, _) = load_harness_settings_or_warn(&self.dirs);
        self.publish_event(
            None,
            Event::HarnessRoleSelected(HarnessRoleSelected {
                baseline_params: selected_model.as_ref().map(|model| {
                    baseline_params_for_selection(
                        &live_settings,
                        &self.provider_model_info,
                        &self.selected_role,
                        model,
                    )
                }),
                model_params: selected_model
                    .as_ref()
                    .map(|model| self.params_for_role_model(&self.selected_role, model))
                    .unwrap_or_default(),
                model: selected_model,
                context_window,
                role: self.selected_role.clone(),
            }),
        );
        self.publish_event(
            None,
            Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
                input_tokens: self.current_session_state.context_input_tokens,
                cached_tokens: self.current_session_state.context_cached_tokens,
                percent_used: self.current_session_state.context_percent_used,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessEffortsAvailable(tau_proto::HarnessEffortsAvailable {
                levels: effort_levels,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessVerbositiesAvailable(tau_proto::HarnessVerbositiesAvailable {
                levels: verbosity_levels,
            }),
        );
        self.publish_event(
            None,
            Event::HarnessThinkingSummariesAvailable(
                tau_proto::HarnessThinkingSummariesAvailable {
                    levels: thinking_levels,
                },
            ),
        );
    }

    fn set_provider_models(&mut self, source_id: &str, models: Vec<ProviderModelInfo>) {
        if models.is_empty() {
            self.provider_models_by_extension.remove(source_id);
        } else {
            self.provider_models_by_extension
                .insert(source_id.to_owned(), models);
        }
        self.refresh_provider_models_and_publish_state();
    }

    fn pending_compaction_targeted_at(&self, cid: &ConversationId) -> bool {
        self.pending_compactions
            .values()
            .any(|pending| pending.target_cid == *cid)
    }

    fn start_auto_compaction_for_conversation(
        &mut self,
        target_cid: &ConversationId,
        resume: PendingCompactionResume,
    ) {
        let Some(target_conv) = self.conversations.get(target_cid) else {
            return;
        };
        let target_head = target_conv.head;
        let target_session_id = target_conv.session_id.clone();
        let target_originator = target_conv.originator.clone();
        let original_input_tokens = target_conv.context_input_tokens.or_else(|| {
            (target_cid == &self.default_conversation_id)
                .then_some(self.current_session_state.context_input_tokens)
                .flatten()
        });
        let summary_cid = ConversationId::new(format!("compact-{}", self.next_session_prompt_id));
        let conv = Conversation::new(
            summary_cid.clone(),
            target_session_id.clone(),
            tau_proto::PromptOriginator::Extension {
                name: HARNESS_CONNECTION_ID.into(),
                query_id: format!("auto-compact-{target_cid}"),
            },
            target_head,
            Some(HARNESS_CONNECTION_ID.into()),
        );
        self.conversations.insert(summary_cid.clone(), conv);
        self.pending_compactions.insert(
            summary_cid.clone(),
            PendingCompaction {
                target_cid: target_cid.clone(),
                session_id: target_session_id.clone(),
                target_agent_id: self.target_agent_id_for_conversation(target_cid),
                originator: target_originator.clone(),
                original_input_tokens,
                resume,
            },
        );
        if let Some(target) = self.conversations.get_mut(target_cid) {
            target.turn_state = ConversationTurnState::Compacting;
        }
        self.publish_event(
            None,
            Event::SessionCompactionStarted(tau_proto::SessionCompactionStarted {
                session_id: target_session_id,
                target_agent_id: self.target_agent_id_for_conversation(target_cid),
                originator: target_originator,
                original_input_tokens,
            }),
        );
        self.dispatch_prompt_after_publish_idle(&summary_cid);
    }

    /// Queue a prompt when it cannot be sent directly yet, or dispatch
    /// it immediately when the session is initialized and the harness is
    /// ready to talk to the agent.
    ///
    /// Rejects prompts whose `session_id` doesn't match the harness's
    /// bound session — one harness owns one session, period. Switching
    /// sessions is a separate (future) operation that tears down +
    /// respawns extensions, not a silent fan-out.
    fn submit_user_prompt(
        &mut self,
        session_id: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HarnessError> {
        if session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; prompt for `{}` rejected",
                self.current_session_id.as_str(),
                session_id.as_str()
            );
            self.emit_info(&reason);
            return Ok(PromptSubmission::Rejected { reason });
        }

        let cid = self.default_conversation_id.clone();
        self.ensure_agent_id_for_conversation(&cid);

        // A user prompt outranks any best-effort side conversation
        // (idle-summary etc.). The agent processes prompts on a
        // single thread, so an in-flight side query stuck in a
        // retry backoff would otherwise stall the user's turn for
        // up to the side conv's full retry budget. Abort first;
        // dispatch second.
        self.preempt_blocking_ext_side_conversations(&session_id);

        if self.dispatch_blocked_for(&cid) || !self.session_initialized(&session_id) {
            self.conversations
                .get_mut(&cid)
                .expect("default conversation always present")
                .pending_prompts
                .push_back(PendingPrompt::user(text));
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }

        self.dispatch_user_prompt(session_id, text)?;
        Ok(PromptSubmission::Dispatched)
    }

    fn submit_prompt_to_agent(
        &mut self,
        session_id: SessionId,
        agent_id: &str,
        text: String,
    ) -> Result<PromptSubmission, HarnessError> {
        if session_id != self.current_session_id {
            let reason = format!(
                "harness is bound to session `{}`; prompt for `{}` rejected",
                self.current_session_id.as_str(),
                session_id.as_str()
            );
            self.emit_info(&reason);
            return Ok(PromptSubmission::Rejected { reason });
        }
        let Some(cid) = self.agent_conversations.get(agent_id).cloned() else {
            self.emit_info(&format!("unknown agent `{agent_id}`"));
            return Ok(PromptSubmission::Rejected {
                reason: format!("unknown agent `{agent_id}`"),
            });
        };
        self.set_agent_state(agent_id, AgentState::Active);
        if self.dispatch_blocked_for(&cid) || !self.session_initialized(&session_id) {
            if let Some(conv) = self.conversations.get_mut(&cid) {
                conv.pending_prompts
                    .push_back(PendingPrompt::user(text.clone()));
            }
            self.publish_event(
                None,
                Event::SessionPromptQueued(SessionPromptQueued {
                    session_id: session_id.clone(),
                    text,
                    target_agent_id: Some(agent_id.to_owned()),
                    message_class: tau_proto::PromptMessageClass::User,
                }),
            );
            self.try_advance_queue();
            return Ok(PromptSubmission::Queued);
        }
        self.dispatch_prompt_for_conversation(&cid, PendingPrompt::user(text))?;
        Ok(PromptSubmission::Dispatched)
    }

    /// Cancel every in-flight non-tool extension side conversation
    /// (idle-summary and friends) so the agent's single prompt slot
    /// is free for the incoming user turn. Delegate sub-agents are
    /// left alone — they're part of an active user turn already and
    /// cancelling them would orphan the parent's tool call.
    ///
    /// Side effects per matching conversation: clear in-flight
    /// state, drop the spid from `prompt_conversations`, mark it
    /// canceled, and publish a terminal prompt lifecycle event. A
    /// targeted `UiCancelPrompt` event is then published so the
    /// agent's retry-sleep wakes and aborts whatever it's currently
    /// processing.
    fn preempt_blocking_ext_side_conversations(&mut self, session_id: &SessionId) {
        let to_cancel: Vec<(ConversationId, SessionId, SessionPromptId, PromptOriginator)> = self
            .conversations
            .iter()
            .filter_map(|(cid, conv)| {
                if cid == &self.default_conversation_id {
                    return None;
                }
                if conv.parent_tool_call_id.is_some() {
                    return None;
                }
                // A compaction summary is also an extension-owned side
                // conversation, but it is not disposable. Its finished
                // response is what restores the target conversation from
                // `Compacting` and drains any queued prompt.
                if self.pending_compactions.contains_key(cid) {
                    return None;
                }
                if !matches!(
                    conv.originator,
                    tau_proto::PromptOriginator::Extension { .. }
                ) {
                    return None;
                }
                let in_flight = conv.in_flight_prompt.clone()?;
                Some((
                    cid.clone(),
                    conv.session_id.clone(),
                    in_flight,
                    conv.originator.clone(),
                ))
            })
            .collect();

        if to_cancel.is_empty() {
            return;
        }

        for (cid, prompt_session_id, spid, originator) in &to_cancel {
            self.canceled_prompts.insert(spid.clone());
            self.prompt_conversations.remove(spid);
            if let Some(conv) = self.conversations.get_mut(cid) {
                conv.in_flight_prompt = None;
                conv.turn_state = ConversationTurnState::Idle;
                conv.pending_prompts.clear();
            }
            self.release_start_agent_request(cid);
            self.publish_prompt_terminated(
                prompt_session_id.clone(),
                spid.clone(),
                SessionPromptTerminationReason::Canceled,
                originator.clone(),
            );
            self.emit_info(&format!(
                "preempting side conv `{cid}` ({spid}) for incoming user prompt",
            ));
            // Targeted cancel per spid. A broadcast cancel
            // (`session_prompt_id: None`) would also abort an
            // unrelated user/delegate prompt that happens to be
            // retry-sleeping on the agent side — the very prompt
            // we're trying to *unblock* by preempting these side
            // convs. Targeted form: the agent only aborts the
            // matching attempt, or records the spid in
            // `canceled_spids` if the prompt is still queued.
            self.publish_event(
                None,
                Event::UiCancelPrompt(UiCancelPrompt {
                    session_id: session_id.clone(),
                    target_agent_id: self.target_agent_id_for_conversation(cid),
                    session_prompt_id: Some(spid.clone()),
                }),
            );
        }
    }

    /// Broadcasts `SessionStarted` for `session_id` and enters
    /// `InitializingSession` until every subscribed tool extension has
    /// acknowledged with `ExtensionContextReady` (or all of them have
    /// disconnected). When the wait set drains for a new/initial session,
    /// AGENTS.md content is injected into the session log and any queued user
    /// prompts are dispatched. Resume keeps the existing transcript intact.
    /// Renders the session tree as one `harness.info` line per node.
    /// Bound-session-only: refuses if `session_id` doesn't match.
    fn handle_tree_request(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            self.emit_info(&format!(
                "tree request for `{}` ignored; harness is bound to `{}`",
                session_id.as_str(),
                self.current_session_id.as_str()
            ));
            return;
        }
        let lines: Vec<String> = match self.store.session(session_id.as_str()) {
            Some(tree) if !tree.nodes().is_empty() => {
                let head = tree.head();
                tree.nodes()
                    .iter()
                    .map(|node| {
                        let marker = if Some(node.id) == head { '*' } else { ' ' };
                        let parent = node
                            .parent_id
                            .map(|p| format!("<- {}", p.get()))
                            .unwrap_or_else(|| "(root)".to_owned());
                        let preview = render_entry_preview(&node.entry);
                        format!(
                            "  {:>3} {} {:>8}  {}",
                            node.id.get(),
                            marker,
                            parent,
                            preview
                        )
                    })
                    .collect()
            }
            _ => {
                self.emit_info(&format!(
                    "session `{}` has no entries yet",
                    session_id.as_str()
                ));
                return;
            }
        };
        for line in lines {
            self.emit_info(&line);
        }
    }

    /// Validates a `UiNavigateTree` request against the bound session.
    /// Returns `true` if the request should be published (and the
    /// resulting `apply_event` will move the head); `false` if it
    /// should be dropped with a user-visible info message. The store
    /// itself has no imperative `set_head`; head moves come from
    /// folding the published `UiNavigateTree` event.
    fn handle_navigate_tree(&mut self, session_id: &SessionId, node_id: u64) -> bool {
        if session_id != &self.current_session_id {
            self.emit_info(&format!(
                "navigate ignored: harness is bound to `{}`",
                self.current_session_id.as_str()
            ));
            return false;
        }
        let valid = self
            .store
            .session(session_id.as_str())
            .and_then(|t| t.node(tau_core::NodeId::new(node_id)))
            .is_some();
        if !valid {
            self.emit_info(&format!("no node `{node_id}` in session"));
            return false;
        }
        self.emit_info(&format!("navigated to node {node_id}"));
        true
    }

    /// Tear down the current session and bind the harness to a new one.
    ///
    /// Pi-style: emit `SessionShutdown` for the old, drop in-flight
    /// prompts, swap the bound id, then run a fresh `start_session_init`
    /// for the new id with the given reason. Extension processes are
    /// kept across sessions (they're not respawned); extensions that
    /// hold per-session state subscribe to `session.shutdown` to
    /// flush/clean up.
    fn switch_session(
        &mut self,
        new_session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) -> Result<(), HarnessError> {
        if new_session_id == self.current_session_id
            && !matches!(reason, tau_proto::SessionStartReason::New)
        {
            self.emit_info(&format!("already on session `{}`", new_session_id.as_str()));
            return Ok(());
        }

        let old_id = self.current_session_id.clone();
        self.publish_event(
            None,
            Event::SessionShutdown(tau_proto::SessionShutdown { session_id: old_id }),
        );

        // Drop in-flight work bound to the old session. Pending prompts
        // for it are abandoned (the user explicitly switched away), and
        // each conversation's per-turn state is reset.
        self.turn_state = TurnState::Idle;
        for conv in self.conversations.values_mut() {
            conv.pending_prompts.clear();
            conv.in_flight_prompt = None;
            conv.turn_state = ConversationTurnState::Idle;
        }
        self.tool_turn.clear();
        self.tool_conversations.clear();
        self.pending_tools.clear();
        self.pending_tool_providers.clear();
        self.pending_action_invocations.clear();
        self.prompt_conversations.clear();
        self.pending_provider_prompts.clear();
        self.pending_compactions.clear();
        self.pending_restore_notice_sessions.clear();
        self.pending_restore_background_notices.clear();
        self.pending_tool_availability_notices.clear();
        self.unavailable_tool_notices_delivered.clear();
        self.pending_start_agent_requests.clear();
        self.active_start_agent_requests.clear();
        self.subagents = SubagentToolState::default();

        // Token and context accounting are session-scoped. Reset them
        // before `SessionStarted` so clients recreating status UI for
        // the new session do not inherit the previous transcript's
        // cumulative totals.
        self.current_session_state = CurrentSessionState::default();

        // Rebind the default conversation to the new session and drop
        // any side conversations that were tied to the old one. Without
        // this, the next `dispatch_user_prompt` would assert because
        // `conversations[default].session_id` still points at the old
        // session id.
        let default_id = self.default_conversation_id.clone();
        let new_head = if matches!(reason, tau_proto::SessionStartReason::New) {
            // `/session new` must start a fresh branch even if the requested
            // id already has durable history (e.g. a short-id
            // collision, or an explicit same-id reset in tests). If
            // we reused the existing tree head, the dedup map would
            // lazily rebuild from old tool results and emit confusing
            // `[tau-internal]` pointers to outputs the model cannot see
            // in the fresh conversation.
            None
        } else {
            self.store
                .load_session(new_session_id.as_str())?
                .and_then(|t| t.head())
        };
        self.conversations.clear();
        self.agent_conversations.clear();
        self.agent_states.clear();
        self.stopped_agent_ids.clear();
        self.conversations.insert(
            default_id.clone(),
            Conversation::new(
                default_id,
                new_session_id.clone(),
                tau_proto::PromptOriginator::User,
                new_head,
                None,
            ),
        );

        self.current_session_id = new_session_id.clone();
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.rehydrate_default_agent_from_session();
            self.rehydrate_agent_states_from_session();
        }
        self.publish_delegate_roles_context();

        // Record cwd + acquire flock on the new session dir before
        // anyone tries to write to its log.
        self.store
            .record_session_meta(new_session_id.as_str(), std::env::current_dir().ok())?;

        // Send the new debug log to the new session's dir, so each
        // session is self-contained.
        let _ = self.enable_debug_log(&self.sessions_dir().join(new_session_id.as_str()));
        self.start_session_init(new_session_id.clone(), reason);
        let session_status = match reason {
            tau_proto::SessionStartReason::Initial | tau_proto::SessionStartReason::New => {
                tau_proto::SessionDirStatus::New
            }
            tau_proto::SessionStartReason::Resume => tau_proto::SessionDirStatus::Resumed,
        };
        self.publish_event(
            None,
            Event::HarnessSessionDir(tau_proto::HarnessSessionDir {
                session_id: new_session_id.clone(),
                path: self.sessions_dir().join(new_session_id.as_str()),
                status: session_status,
            }),
        );
        Ok(())
    }

    fn sessions_dir(&self) -> PathBuf {
        // The harness doesn't currently store the sessions dir directly;
        // derive it from the session store's location. SessionStore
        // exposes its root via the `sessions_dir()` accessor.
        self.store.sessions_dir().to_path_buf()
    }

    fn internal_prompt_already_persisted(&self, session_id: &SessionId, text: &str) -> bool {
        self.store
            .session_events(session_id.as_str())
            .map(|events| {
                events
                    .iter()
                    .any(|entry| event_is_internal_prompt_text(&entry.event, text))
            })
            .unwrap_or(false)
    }

    fn restore_notice_already_persisted(&self, session_id: &SessionId) -> bool {
        self.store
            .session_events(session_id.as_str())
            .map(|events| {
                events
                    .iter()
                    .any(|entry| event_is_internal_restore_notice(&entry.event))
            })
            .unwrap_or(false)
    }

    fn last_recorded_session_event_at(
        &self,
        session_id: &SessionId,
    ) -> Option<tau_proto::UnixMicros> {
        self.store
            .session_events(session_id.as_str())
            .ok()?
            .iter()
            .rev()
            .find_map(|entry| (entry.recorded_at.get() != 0).then_some(entry.recorded_at))
    }

    fn queue_restore_notice_for_resumed_session(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            return;
        }
        if self.restore_notice_already_persisted(session_id) {
            self.pending_restore_notice_sessions.remove(session_id);
            return;
        }
        let last_recorded_at = self.last_recorded_session_event_at(session_id);
        self.pending_restore_notice_sessions
            .insert(session_id.clone(), last_recorded_at);
    }

    fn queue_restore_background_notices_for_resumed_session(&mut self, session_id: &SessionId) {
        if session_id != &self.current_session_id {
            return;
        }
        let mut seen = HashSet::new();
        let mut notices = Vec::new();
        for state in self.restored_background_tool_states(session_id) {
            let Some(tau_core::BackgroundToolCompletion::Error(error)) = state.completion else {
                continue;
            };
            let notice = restored_background_tool_call_error_message(&error.call_id);
            if error.message != notice || !seen.insert(notice.clone()) {
                continue;
            }
            if self.internal_prompt_already_persisted(session_id, &notice) {
                continue;
            }
            notices.push(notice);
        }
        if notices.is_empty() {
            self.pending_restore_background_notices.remove(session_id);
        } else {
            self.pending_restore_background_notices
                .insert(session_id.clone(), notices);
        }
    }

    fn mark_tool_unavailable_for_notice(
        &mut self,
        internal_name: ToolName,
        visible_name: ToolName,
    ) {
        let internal_name = internal_name.into_string();
        if matches!(
            self.pending_tool_availability_notices.get(&internal_name),
            Some(PendingToolAvailabilityNotice::Unavailable { .. })
        ) {
            return;
        }
        if matches!(
            self.pending_tool_availability_notices.get(&internal_name),
            Some(PendingToolAvailabilityNotice::AvailableAgain { .. })
        ) {
            self.pending_tool_availability_notices
                .remove(&internal_name);
            return;
        }
        if self
            .unavailable_tool_notices_delivered
            .contains_key(&internal_name)
        {
            return;
        }
        self.pending_tool_availability_notices.insert(
            internal_name,
            PendingToolAvailabilityNotice::Unavailable { visible_name },
        );
    }

    fn mark_tool_available_for_notice(&mut self, internal_name: ToolName, visible_name: ToolName) {
        let internal_name = internal_name.into_string();
        if matches!(
            self.pending_tool_availability_notices.get(&internal_name),
            Some(PendingToolAvailabilityNotice::Unavailable { .. })
        ) {
            self.pending_tool_availability_notices
                .remove(&internal_name);
            return;
        }
        if self
            .unavailable_tool_notices_delivered
            .contains_key(&internal_name)
        {
            self.pending_tool_availability_notices.insert(
                internal_name,
                PendingToolAvailabilityNotice::AvailableAgain { visible_name },
            );
        }
    }

    fn take_pending_tool_availability_prompts_for_user_prompt(&mut self) -> Vec<PendingPrompt> {
        let pending = std::mem::take(&mut self.pending_tool_availability_notices);
        let mut prompts = Vec::new();
        for (internal_name, notice) in pending {
            match &notice {
                PendingToolAvailabilityNotice::Unavailable { visible_name } => {
                    self.unavailable_tool_notices_delivered
                        .insert(internal_name, visible_name.clone());
                }
                PendingToolAvailabilityNotice::AvailableAgain { .. } => {
                    self.unavailable_tool_notices_delivered
                        .remove(&internal_name);
                }
            }
            prompts.push(PendingPrompt::internal(notice.prompt_text()));
        }
        prompts
    }

    /// Consume pending internal notices for the default conversation, if the
    /// next prompt is a real user prompt on the current session.
    pub(crate) fn take_pending_restore_prompts_for_user_prompt(
        &mut self,
        cid: &ConversationId,
    ) -> Vec<PendingPrompt> {
        if cid != &self.default_conversation_id {
            return Vec::new();
        }
        let Some(session_id) = self
            .conversations
            .get(cid)
            .map(|conv| conv.session_id.clone())
        else {
            return Vec::new();
        };
        if session_id != self.current_session_id {
            return Vec::new();
        }

        let mut prompts = Vec::new();
        if self.restore_notice_already_persisted(&session_id) {
            self.pending_restore_notice_sessions.remove(&session_id);
        } else if let Some(last_recorded_at) =
            self.pending_restore_notice_sessions.remove(&session_id)
        {
            prompts.push(PendingPrompt::internal(restore_notice_prompt(
                last_recorded_at,
                tau_proto::UnixMicros::now(),
            )));
        }

        if let Some(notices) = self.pending_restore_background_notices.remove(&session_id) {
            for notice in notices {
                if !self.internal_prompt_already_persisted(&session_id, &notice) {
                    prompts.push(PendingPrompt::internal(notice));
                }
            }
        }
        prompts.extend(self.take_pending_tool_availability_prompts_for_user_prompt());
        prompts
    }

    fn repair_restored_foreground_tool_calls(&mut self, session_id: &SessionId) -> usize {
        if session_id != &self.current_session_id {
            return 0;
        }
        let cid = self.default_conversation_id.clone();
        let Some(head) = self.conversations.get(&cid).map(|conv| conv.head) else {
            return 0;
        };
        let calls: Vec<ToolCallItem> = self
            .store
            .session(session_id.as_str())
            .map(|tree| {
                tree.unresolved_foreground_tool_calls_from(head)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let count = calls.len();
        for call in calls {
            let error = ToolError {
                call_id: call.call_id.clone(),
                tool_name: call.name,
                tool_type: call.tool_type,
                message: restored_tool_call_error_message(&call.call_id),
                details: None,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            };
            self.publish_terminal_tool_error(Some(&cid), Some(HARNESS_CONNECTION_ID), error);
        }
        count
    }

    fn restored_background_tool_states(
        &self,
        session_id: &SessionId,
    ) -> Vec<tau_core::BackgroundToolCallState> {
        if session_id != &self.current_session_id {
            return Vec::new();
        }
        let Some(head) = self
            .conversations
            .get(&self.default_conversation_id)
            .map(|conv| conv.head)
        else {
            return Vec::new();
        };
        let Ok(events) = self.store.session_events(session_id.as_str()) else {
            return Vec::new();
        };
        self.store
            .session(session_id.as_str())
            .map(|tree| tree.background_tool_calls_from(head, &events))
            .unwrap_or_default()
    }

    fn seed_restored_wait_background_completions(&mut self, session_id: &SessionId) {
        for state in self.restored_background_tool_states(session_id) {
            match state.completion {
                Some(tau_core::BackgroundToolCompletion::Result(result)) => {
                    self.record_wait_background_result(result);
                }
                Some(tau_core::BackgroundToolCompletion::Error(error)) => {
                    self.record_wait_background_error(error);
                }
                None => {}
            }
        }
    }

    fn repair_restored_background_tool_calls(&mut self, session_id: &SessionId) -> usize {
        if session_id != &self.current_session_id {
            return 0;
        }
        let cid = self.default_conversation_id.clone();
        let Some(head) = self.conversations.get(&cid).map(|conv| conv.head) else {
            return 0;
        };
        let Ok(events) = self.store.session_events(session_id.as_str()) else {
            return 0;
        };
        let calls = self
            .store
            .session(session_id.as_str())
            .map(|tree| tree.unresolved_background_tool_calls_from(head, &events))
            .unwrap_or_default();
        let count = calls.len();
        for call in calls {
            let error = ToolBackgroundError {
                call_id: call.call_id.clone(),
                tool_name: call.tool_name,
                tool_type: call.tool_type,
                message: restored_background_tool_call_error_message(&call.call_id),
                details: None,
                display: None,
                originator: call.originator,
            };
            self.publish_terminal_background_error(&cid, Some(HARNESS_CONNECTION_ID), error);
        }
        count
    }

    fn repair_restored_session_tool_state(&mut self, session_id: &SessionId) {
        self.repair_restored_foreground_tool_calls(session_id);
        self.repair_restored_background_tool_calls(session_id);
        self.seed_restored_wait_background_completions(session_id);
        self.queue_restore_background_notices_for_resumed_session(session_id);
    }

    pub(crate) fn start_session_init(
        &mut self,
        session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) {
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.queue_restore_notice_for_resumed_session(&session_id);
        }
        let waiting_on = self.session_init_provider_ids();
        if waiting_on.is_empty() {
            if matches!(reason, tau_proto::SessionStartReason::Resume) {
                self.repair_restored_session_tool_state(&session_id);
            }
            if let Err(error) = self.complete_session_init(session_id, reason) {
                self.emit_info(&format!("failed to initialize session: {error}"));
                self.turn_state = TurnState::Idle;
            }
            return;
        }

        for source_id in &waiting_on {
            self.remove_discovered_context(source_id.as_str());
        }

        self.turn_state = TurnState::InitializingSession {
            session_id: session_id.clone(),
            reason,
            waiting_on,
        };
        self.publish_event(
            None,
            Event::SessionStarted(tau_proto::SessionStarted {
                session_id: session_id.clone(),
                reason,
            }),
        );
        if matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.repair_restored_session_tool_state(&session_id);
        }
    }

    fn handle_extension_context_ready(
        &mut self,
        source_id: &str,
        ready: tau_proto::ExtensionContextReady,
    ) -> Result<(), HarnessError> {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                reason,
                waiting_on,
            } if *session_id == ready.session_id => {
                waiting_on.remove(source_id);
                waiting_on.is_empty().then(|| (session_id.clone(), *reason))
            }
            _ => None,
        };

        if let Some((session_id, reason)) = completed_session {
            self.complete_session_init(session_id, reason)?;
        }

        Ok(())
    }

    fn maybe_complete_session_init_for_disconnect(&mut self, connection_id: &str) {
        let completed_session = match &mut self.turn_state {
            TurnState::InitializingSession {
                session_id,
                reason,
                waiting_on,
            } => {
                let removed = waiting_on.remove(connection_id);
                if removed && waiting_on.is_empty() {
                    Some((session_id.clone(), *reason))
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some((session_id, reason)) = completed_session
            && let Err(error) = self.complete_session_init(session_id, reason)
        {
            self.emit_info(&format!("failed to initialize session: {error}"));
            self.turn_state = TurnState::Idle;
        }
    }

    fn complete_session_init(
        &mut self,
        session_id: SessionId,
        reason: tau_proto::SessionStartReason,
    ) -> Result<(), HarnessError> {
        // A resumed session already has its model-visible startup context in
        // history. Re-injecting AGENTS.md would append a duplicate user message;
        // restore/tool notices are queued separately before the next real prompt.
        if !matches!(reason, tau_proto::SessionStartReason::Resume) {
            self.ensure_agents_context_inserted(session_id.as_str())?;
        }
        // No explicit head sync needed: when the AGENTS.md
        // injection is for `current_session_id` it's stamped with
        // `default_conversation_id` and the post-commit hook keeps
        // `c.head` aligned. For other sessions there's no
        // matching live conversation anyway.
        self.initialized_sessions.insert(session_id.clone());
        self.request_prompt_prewarm(&session_id);
        self.turn_state = TurnState::Idle;
        self.try_advance_queue();
        Ok(())
    }

    fn request_prompt_prewarm(&mut self, session_id: &SessionId) {
        let Some(model) = self.selected_model.clone() else {
            tracing::debug!(
                target: "harness",
                session_id = %session_id,
                "skipping prompt prewarm: no selected model",
            );
            return;
        };
        if session_id != &self.current_session_id {
            tracing::debug!(
                target: "harness",
                session_id = %session_id,
                "skipping prompt prewarm: session is not bound to this harness",
            );
            return;
        }

        let cid = self.default_conversation_id.clone();
        let Some(conv) = self.conversations.get(&cid) else {
            tracing::debug!(
                target: "harness",
                session_id = %session_id,
                "skipping prompt prewarm: default conversation missing",
            );
            return;
        };
        let head = conv.head;
        let tree = self.store.session(session_id.as_str());
        let context_items = tree
            .map(|t| assemble_conversation_from(t, head))
            .unwrap_or_default();
        let tools = self.gather_tool_definitions();
        let system_prompt = self.build_current_system_prompt();
        let event = Event::SessionPromptPrewarmRequested(SessionPromptPrewarmRequested {
            session_id: session_id.clone(),
            system_prompt,
            context_items,
            tools,
            model: Some(model),
            model_params: self
                .selected_model
                .as_ref()
                .map(|model| self.params_for_role_model(&self.selected_role, model))
                .unwrap_or_default(),
            tool_choice: tau_proto::ToolChoice::Auto,
            originator: tau_proto::PromptOriginator::User,
            share_user_cache_key: false,
        });
        tracing::debug!(
            target: "harness",
            session_id = %session_id,
            "scheduled prompt prewarm",
        );
        self.publish_event(None, event);
    }

    // -----------------------------------------------------------------------
    // Agent prompt assembly
    // -----------------------------------------------------------------------

    fn ensure_agents_context_inserted(&mut self, session_id: &str) -> Result<(), HarnessError> {
        if self.discovered_agents_files.is_empty() {
            return Ok(());
        }

        let text = render_agents_context_message(self.discovered_agents_files.iter());
        let event = Event::SessionUserMessageInjected(tau_proto::SessionUserMessageInjected {
            session_id: session_id.into(),
            text,
            message_class: tau_proto::PromptMessageClass::User,
        });
        // Publish the injection as an event so it reaches the durable
        // session log and folds into the SessionTree the same way
        // every other entry does. Stamp it with the default
        // conversation when the session matches, so the post-commit
        // hook syncs `c.head` automatically — replaces the old
        // `sync_default_conversation_head()` call site that raced
        // when SessionUserMessageInjected was intercepted.
        if self.current_session_id == session_id {
            let cid = self.default_conversation_id.clone();
            self.publish_event_for_conversation(&cid, None, event);
        } else {
            self.publish_event(None, event);
        }
        Ok(())
    }

    /// Persist a user-initiated `!` shell command's output as a
    /// tagged user message so the agent sees it in the next prompt.
    ///
    /// The XML-ish `<user_shell>` envelope lets the model reliably
    /// distinguish output the user pasted vs. output from its own
    /// tool calls, and survives round-tripping through conversation
    /// assembly.
    fn inject_user_shell_output(&mut self, finished: &tau_proto::ShellCommandFinished) {
        let exit = finished
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| if finished.cancelled { "cancelled" } else { "?" }.to_owned());
        let text = format!(
            "<user_shell command={:?} exit_code={:?}>\n{}\n</user_shell>",
            finished.command, exit, finished.output,
        );
        let event = Event::SessionUserMessageInjected(tau_proto::SessionUserMessageInjected {
            session_id: finished.session_id.clone(),
            text,
            message_class: tau_proto::PromptMessageClass::User,
        });
        // When the shell output belongs to the bound session, stamp
        // the publish with the target conversation so the fold lands
        // on the branch whose transcript owned the command (and the
        // post-commit hook syncs `c.head`). Other sessions:
        // best-effort plain publish; nothing on this harness instance
        // is reading their tree.
        if finished.session_id == self.current_session_id {
            if let Some(cid) =
                self.conversation_id_for_target_agent(finished.target_agent_id.as_deref())
            {
                self.publish_event_for_conversation(&cid, None, event);
            } else {
                self.publish_event(None, event);
            }
        } else {
            self.publish_event(None, event);
        }
    }

    /// Convenience wrapper that dispatches a prompt for the harness's
    /// default (user) conversation. Used by tests that want a quick
    /// "send the next prompt" without going through the full
    /// dispatch pipeline.
    #[cfg(test)]
    fn send_prompt_to_agent(&mut self, session_id: &str) -> SessionPromptId {
        debug_assert_eq!(
            self.conversations[&self.default_conversation_id]
                .session_id
                .as_str(),
            session_id,
            "send_prompt_to_agent only valid for the default conversation; \
             use send_prompt_to_agent_for() for side conversations",
        );
        let cid = self.default_conversation_id.clone();
        self.send_prompt_to_agent_for(&cid)
    }

    fn set_agent_state(&mut self, agent_id: &str, state: AgentState) {
        if agent_id == "main" {
            return;
        }
        if self.agent_states.get(agent_id).copied() == Some(state) {
            return;
        }
        self.agent_states.insert(agent_id.to_owned(), state);
        self.publish_event(
            None,
            Event::SessionAgentStateChanged(SessionAgentStateChanged {
                session_id: self.current_session_id.clone(),
                agent_id: agent_id.to_owned(),
                state,
            }),
        );
    }

    fn remove_conversation(&mut self, cid: &ConversationId) -> Option<Conversation> {
        if let Some(conv) = self.conversations.get(cid)
            && let Some(agent_id) = &conv.agent_id
        {
            self.agent_conversations.remove(agent_id);
            self.agent_states.remove(agent_id);
            self.stopped_agent_ids.insert(agent_id.clone());
        }
        self.conversations.remove(cid)
    }

    fn rotate_default_agent_conversation(&mut self) {
        let cid = self.default_conversation_id.clone();
        let archive_cid = self.archive_default_conversation_id();
        if let Some(mut conversation) = self.conversations.remove(&cid) {
            conversation.id = archive_cid.clone();
            if let Some(agent_id) = conversation.agent_id.clone() {
                self.agent_conversations
                    .insert(agent_id, archive_cid.clone());
            }
            self.conversations.insert(archive_cid.clone(), conversation);
            self.rewrite_conversation_references(&cid, &archive_cid);
        }
        self.conversations.insert(
            cid.clone(),
            Conversation::new(
                cid,
                self.current_session_id.clone(),
                tau_proto::PromptOriginator::User,
                None,
                None,
            ),
        );
    }

    fn archive_default_conversation_id(&self) -> ConversationId {
        for index in 0_u64.. {
            let candidate = ConversationId::new(format!("archived-default-{index}"));
            if !self.conversations.contains_key(&candidate) {
                return candidate;
            }
        }
        unreachable!("unbounded archive conversation id search cannot exhaust")
    }

    fn rewrite_conversation_references(&mut self, old: &ConversationId, new: &ConversationId) {
        for cid in self.tool_conversations.values_mut() {
            Self::rewrite_conversation_id(cid, old, new);
        }
        for cid in self.prompt_conversations.values_mut() {
            Self::rewrite_conversation_id(cid, old, new);
        }
        for conversation in self.conversations.values_mut() {
            Self::rewrite_optional_conversation_id(
                &mut conversation.parent_conversation_id,
                old,
                new,
            );
        }
        for cid in self.agent_conversations.values_mut() {
            Self::rewrite_conversation_id(cid, old, new);
        }
        for cid in &mut self.pending_user_prompt_dispatches {
            Self::rewrite_conversation_id(cid, old, new);
        }
        for cid in &mut self.pending_publish_idle_dispatches {
            Self::rewrite_conversation_id(cid, old, new);
        }
        for cid in self.background_completion_targets.values_mut() {
            Self::rewrite_conversation_id(cid, old, new);
        }
        for pending in self.pending_compactions.values_mut() {
            Self::rewrite_conversation_id(&mut pending.target_cid, old, new);
        }
        for pending in &mut self.pending_start_agent_requests {
            Self::rewrite_conversation_id(&mut pending.cid, old, new);
            Self::rewrite_conversation_id(&mut pending.parent_cid, old, new);
        }
        if let Some(pending) = &mut self.pending_intercept {
            Self::rewrite_conversation_head_sync(&mut pending.sync_head_for, old, new);
        }
        for deferred in &mut self.deferred_publishes {
            Self::rewrite_conversation_head_sync(&mut deferred.sync_head_for, old, new);
        }
        self.tool_turn.rewrite_conversation_id(old, new);
        self.subagents.rewrite_conversation_id(old, new);
        if let Some(pending) = self.pending_compactions.remove(old) {
            self.pending_compactions.insert(new.clone(), pending);
        }
        if let Some(active) = self.active_start_agent_requests.remove(old) {
            self.active_start_agent_requests.insert(new.clone(), active);
        }
    }

    fn rewrite_conversation_id(
        cid: &mut ConversationId,
        old: &ConversationId,
        new: &ConversationId,
    ) {
        if cid == old {
            *cid = new.clone();
        }
    }

    fn rewrite_optional_conversation_id(
        cid: &mut Option<ConversationId>,
        old: &ConversationId,
        new: &ConversationId,
    ) {
        if cid.as_ref() == Some(old) {
            *cid = Some(new.clone());
        }
    }

    fn rewrite_conversation_head_sync(
        sync: &mut Option<ConversationHeadSync>,
        old: &ConversationId,
        new: &ConversationId,
    ) {
        if let Some(sync) = sync {
            Self::rewrite_conversation_id(&mut sync.cid, old, new);
        }
    }

    fn rehydrate_default_agent_from_session(&mut self) {
        let Ok(events) = self.store.session_events(self.current_session_id.as_str()) else {
            return;
        };
        let target_agent_id = events.iter().rev().find_map(|entry| match &entry.event {
            Event::UiPromptSubmitted(prompt) if prompt.originator.is_user() => {
                prompt.target_agent_id.clone()
            }
            Event::SessionPromptCreated(prompt) if prompt.originator.is_user() => {
                prompt.target_agent_id.clone()
            }
            Event::ProviderResponseFinished(finished) if finished.originator.is_user() => {
                finished.target_agent_id.clone()
            }
            Event::SessionCompactionStarted(started) if started.originator.is_user() => {
                started.target_agent_id.clone()
            }
            Event::SessionCompactionFinished(finished) if finished.originator.is_user() => {
                finished.target_agent_id.clone()
            }
            Event::SessionCompacted(compacted) if compacted.originator.is_user() => {
                compacted.target_agent_id.clone()
            }
            _ => None,
        });
        let Some(agent_id) = target_agent_id else {
            return;
        };
        if let Some(conv) = self.conversations.get_mut(&self.default_conversation_id) {
            conv.agent_id = Some(agent_id.clone());
        }
        self.agent_conversations
            .insert(agent_id, self.default_conversation_id.clone());
    }

    fn rehydrate_agent_states_from_session(&mut self) {
        let Ok(events) = self.store.session_events(self.current_session_id.as_str()) else {
            return;
        };
        let mut explicitly_tracked = HashSet::new();
        for entry in events {
            match entry.event {
                Event::SessionAgentStateChanged(changed) => {
                    explicitly_tracked.insert(changed.agent_id.clone());
                    self.agent_states.insert(changed.agent_id, changed.state);
                }
                Event::UiPromptSubmitted(prompt) if prompt.originator.is_user() => {
                    if let Some(agent_id) = prompt.target_agent_id
                        && !explicitly_tracked.contains(agent_id.as_str())
                    {
                        self.agent_states
                            .entry(agent_id)
                            .or_insert(AgentState::Active);
                    }
                }
                Event::SessionPromptCreated(prompt) if prompt.originator.is_user() => {
                    if let Some(agent_id) = prompt.target_agent_id
                        && !explicitly_tracked.contains(agent_id.as_str())
                    {
                        self.agent_states
                            .entry(agent_id)
                            .or_insert(AgentState::Active);
                    }
                }
                Event::ProviderResponseFinished(finished) if finished.originator.is_user() => {
                    if let Some(agent_id) = finished.target_agent_id
                        && !explicitly_tracked.contains(agent_id.as_str())
                    {
                        self.agent_states
                            .entry(agent_id)
                            .or_insert(AgentState::Active);
                    }
                }
                _ => {}
            }
        }
        self.agent_states
            .retain(|agent_id, _| self.agent_conversations.contains_key(agent_id));
    }

    fn agent_id_is_taken(&self, agent_id: &str) -> bool {
        self.agent_conversations.contains_key(agent_id)
            || self.stopped_agent_ids.contains(agent_id)
            || self
                .pending_start_agent_requests
                .iter()
                .any(|pending| pending.agent_id == agent_id)
    }

    pub(crate) fn mint_available_agent_id_for_role(&self, role: &str) -> String {
        mint_available_agent_id_for_role_with(
            role,
            |agent_id| self.agent_id_is_taken(agent_id),
            random_agent_id_suffix,
        )
    }

    pub(crate) fn ensure_agent_id_for_conversation(
        &mut self,
        cid: &ConversationId,
    ) -> Option<String> {
        let role = {
            let conv = self.conversations.get(cid)?;
            if let Some(agent_id) = &conv.agent_id {
                let agent_id = agent_id.clone();
                if self.agent_states.get(&agent_id).copied() != Some(AgentState::ActiveDelegated) {
                    self.set_agent_state(&agent_id, AgentState::Active);
                }
                return Some(agent_id);
            }
            self.role_name_for_conversation(conv)
        };
        let agent_id = self.mint_available_agent_id_for_role(&role);
        if let Some(conv) = self.conversations.get_mut(cid) {
            conv.agent_id = Some(agent_id.clone());
        }
        self.stopped_agent_ids.remove(&agent_id);
        self.agent_conversations
            .insert(agent_id.clone(), cid.clone());
        self.set_agent_state(&agent_id, AgentState::Active);
        Some(agent_id)
    }

    /// Mints a new `SessionPromptId`, registers it with `cid`'s
    /// conversation, and dispatches either a normal
    /// `SessionPromptCreated` or a `SessionCompactionRequested` to the
    /// agent. Reads `system_prompt` / `messages` / `tools` from the
    /// conversation's session tree.
    ///
    /// Linear-prefix invariant: each subsequent prompt for the same
    /// session must be a strict byte-prefix extension of the prior
    /// one. Provider prompt caches (OpenAI, Anthropic, etc.) key
    /// entirely off the prefix bytes, so any per-turn churn in
    /// `system_prompt`, `tools`, or earlier messages busts the cache.
    /// See `linear_session_prompts_strictly_extend_previous_messages`.
    pub(crate) fn send_prompt_to_agent_for(&mut self, cid: &ConversationId) -> SessionPromptId {
        let _ = self.ensure_agent_id_for_conversation(cid);
        let conv = self
            .conversations
            .get(cid)
            .expect("send_prompt_to_agent_for: unknown conversation id");
        let session_id = conv.session_id.clone();
        let originator = conv.originator.clone();
        let role_name = self.role_name_for_conversation(conv);
        let (prompt_model, prompt_params) = if conv.role.is_some() {
            let model = self.model_for_conversation_role(conv);
            let params = model
                .as_ref()
                .map(|model| self.params_for_role_model(&role_name, model))
                .unwrap_or_default();
            (model, params)
        } else {
            (
                self.selected_model.clone(),
                self.selected_model
                    .as_ref()
                    .map(|model| self.params_for_role_model(&self.selected_role, model))
                    .unwrap_or_default(),
            )
        };
        // Non-tool extension side conversations (`std-notifications`'
        // idle summary, etc.) must not execute tools — their whole
        // job is to produce a one-line summary, and unfettered tool
        // access has historically caused destructive `write`/`edit`
        // calls. Do NOT enforce that by flipping the provider
        // `tool_choice` to `none`: `tool_choice` is serialized on the
        // wire and changing it breaks the request-body equivalence the
        // `previous_response_id` cache relies on. Keep the wire
        // request identical to the parent (`Auto`) and enforce the
        // no-tools rule locally before dispatching any returned tool
        // calls.
        let is_non_tool_ext_query = matches!(
            conv.originator,
            tau_proto::PromptOriginator::Extension { .. }
        ) && conv.parent_tool_call_id.is_none();
        let tool_choice = tau_proto::ToolChoice::Auto;
        // Single-shot side queries (idle-summary) reuse the user's
        // `prompt_cache_key` bucket so they hit the user's warm
        // prefix cache instead of cold-starting their own. Delegate
        // sub-agents keep the per-extension split — they fan out and
        // could overflow the user's bucket past OpenAI's 15 RPM
        // routing guideline.
        let share_user_cache_key = is_non_tool_ext_query;
        // Walk the conversation's *own* branch, not whatever tree.head
        // currently points at. With multiple side conversations
        // running concurrently their tree mutations interleave, so
        // tree.head is an unreliable signal for "where this
        // conversation lives". Reading from `conv.head` keeps the
        // assembled prompt scoped to this conversation's history and
        // prevents orphan ToolUse blocks from cross-branch state.
        let head = conv.head;

        let tree = self.store.session(session_id.as_str());
        let prompt_context = tree
            .map(|t| assemble_prompt_context_from(t, head))
            .unwrap_or_else(|| crate::prompt::AssembledPromptContext {
                context_items: Vec::new(),
            });
        let context_items = prompt_context.context_items;
        let tools = self.gather_tool_definitions_for_role(&role_name);
        let system_prompt = self.build_system_prompt_for_role(&role_name);
        // Fingerprint the non-input fields of the impending request.
        // Used to (a) drop the chain anchor when any of those fields
        // drifted since the anchor was minted (matches Pi's
        // `requestBodiesMatchExceptInput` check, catches divergence
        // before the round-trip), and (b) stamp the next anchor at
        // response time so a future send can repeat the comparison.
        let request_fingerprint = crate::conversation::compute_chain_fingerprint_detail(
            &system_prompt,
            &tools,
            &prompt_params,
            tool_choice,
        );
        // Stateful-chain hint: if the prior turn for this conversation
        // produced a provider response id AND the anchor is still consistent
        // (same model selected, anchor node still on the path to
        // current head, item_count not larger than the assembled
        // count, and the request body's non-input fields haven't
        // drifted) we let the next turn run as a delta call. Otherwise
        // drop the anchor — the chain is busted and full replay is
        // the safe fallback. We resolve this BEFORE moving on so we
        // can clear an invalidated anchor in the same pass.
        let previous_response = {
            let conv = self
                .conversations
                .get(cid)
                .expect("send_prompt_to_agent_for: unknown conversation id");
            let anchor = conv.chain_anchor.as_ref();
            if let Some(a) = anchor {
                let model_ok = prompt_model.as_ref() == Some(&a.model);
                let count_ok = a.message_count <= context_items.len();
                let tree_ok = tree.is_some_and(|t| anchor_is_ancestor(t, a.head, conv.head));
                let fingerprint_ok = a.request_fingerprint == request_fingerprint.digest;
                if model_ok && count_ok && tree_ok && fingerprint_ok {
                    Some(tau_proto::PreviousResponseCandidate {
                        provider_response_id: a.response_id.clone(),
                        next_item_index: a.message_count,
                        backend: a.backend.clone(),
                    })
                } else {
                    tracing::debug!(
                        target: "tau_harness",
                        conversation_id = %cid,
                        session_id = %session_id,
                        response_id = %a.response_id,
                        anchor_model = %a.model,
                        current_model = ?prompt_model,
                        model_ok,
                        anchor_message_count = a.message_count,
                        current_message_count = context_items.len(),
                        count_ok,
                        anchor_head = ?a.head,
                        current_head = ?conv.head,
                        tree_ok,
                        fingerprint_ok,
                        fingerprint_system_prompt_ok = a.request_fingerprint_parts.system_prompt
                            == request_fingerprint.parts.system_prompt,
                        fingerprint_tools_ok = a.request_fingerprint_parts.tools
                            == request_fingerprint.parts.tools,
                        fingerprint_model_params_ok = a.request_fingerprint_parts.model_params
                            == request_fingerprint.parts.model_params,
                        fingerprint_tool_choice_ok = a.request_fingerprint_parts.tool_choice
                            == request_fingerprint.parts.tool_choice,
                        "dropping stale previous_response_id chain anchor",
                    );
                    None
                }
            } else {
                None
            }
        };
        if previous_response.is_none() {
            // Drop a stale anchor so we don't keep re-checking it.
            if let Some(conv) = self.conversations.get_mut(cid) {
                conv.chain_anchor = None;
            }
        }
        let session_prompt_id: SessionPromptId =
            format!("sp-{}", self.next_session_prompt_id).into();
        self.next_session_prompt_id += 1;
        self.prompt_conversations
            .insert(session_prompt_id.clone(), cid.clone());
        let ctx_id = self
            .conversations
            .get_mut(cid)
            .and_then(|c| c.next_ctx_id.take());
        if let Some(c) = self.conversations.get_mut(cid) {
            c.in_flight_prompt = Some(session_prompt_id.clone());
            c.turn_state = ConversationTurnState::AgentThinking {
                session_prompt_id: session_prompt_id.clone(),
            };
        }

        // Publish the prompt-shaped request event. Normal turns use
        // `SessionPromptCreated`; provider-side compaction uses the
        // dedicated `SessionCompactionRequested` envelope.
        let model = prompt_model;
        if let Some(model) = model.as_ref() {
            self.current_session_state.token_usage.start_request(model);
            self.prompt_models
                .insert(session_prompt_id.clone(), model.clone());
        }
        // Stash the fingerprint of what we're about to send so the
        // chain anchor we mint at response time records the body that
        // was actually on the wire — defends against a setting flip
        // racing the response.
        self.prompt_fingerprints
            .insert(session_prompt_id.clone(), request_fingerprint);
        let is_compaction_request = self.pending_compactions.contains_key(cid);
        if !is_compaction_request {
            self.prompt_cache_diagnostics.insert(
                session_prompt_id.clone(),
                PromptCacheDiagnosticContext {
                    model: model.clone(),
                    previous_response: previous_response.clone(),
                    originator: originator.clone(),
                    tool_choice,
                    request_fingerprint: request_fingerprint.digest,
                },
            );
        }
        let target_agent_id = if is_compaction_request {
            self.pending_compactions
                .get(cid)
                .and_then(|pending| pending.target_agent_id.clone())
        } else {
            self.target_agent_id_for_conversation(cid)
        };
        let prompt = SessionPromptCreated {
            session_prompt_id: session_prompt_id.clone(),
            session_id,
            target_agent_id,
            system_prompt,
            context_items,
            tools,
            tools_ref: None,
            model,
            model_params: prompt_params,
            tool_choice,
            originator,
            share_user_cache_key,
            ctx_id,
            previous_response_candidate: previous_response,
        };
        let event = if is_compaction_request {
            Event::SessionCompactionRequested(SessionCompactionRequested {
                prompt: SessionPromptCreated {
                    ctx_id: None,
                    previous_response_candidate: None,
                    ..prompt
                },
            })
        } else {
            Event::SessionPromptCreated(prompt)
        };
        self.publish_event(None, event);

        session_prompt_id
    }

    fn role_name_for_conversation(&self, conv: &Conversation) -> String {
        conv.role
            .clone()
            .unwrap_or_else(|| self.selected_role.clone())
    }

    fn role_name_for_conversation_id(&self, cid: &ConversationId) -> String {
        self.conversations
            .get(cid)
            .and_then(|conv| conv.role.clone())
            .unwrap_or_else(|| self.selected_role.clone())
    }

    fn model_for_conversation_role(&self, conv: &Conversation) -> Option<ModelId> {
        let role_name = self.role_name_for_conversation(conv);
        model_for_role(&self.provider_model_info, &self.available_roles, &role_name)
    }

    pub(crate) fn selected_model_params(&self) -> tau_proto::ModelParams {
        self.selected_model
            .as_ref()
            .map(|model| self.params_for_role_model(&self.selected_role, model))
            .unwrap_or_default()
    }

    fn params_for_role_model(&self, role_name: &str, model: &ModelId) -> tau_proto::ModelParams {
        selected_params_for_role(
            &self.provider_model_info,
            &self.available_roles,
            role_name,
            model,
        )
    }

    fn build_current_system_prompt(&self) -> String {
        self.build_system_prompt_for_role(&self.selected_role)
    }

    fn build_system_prompt_for_role(&self, role_name: &str) -> String {
        let (prompt_fragments, tool_prompt_fragments) =
            self.gather_prompt_fragment_groups_for_role(role_name);
        let system_template = self.system_template_for_role(role_name);
        build_system_prompt_with_tool_template_context(
            system_template,
            &self.discovered_skills,
            &prompt_fragments,
            &tool_prompt_fragments,
            self.session_context
                .template_value(&self.current_session_id),
            RolePromptTemplateContext { role_name },
        )
    }

    fn system_template_for_role(&self, role_name: &str) -> &str {
        let template_name = self
            .available_roles
            .get(role_name)
            .and_then(|role| role.prompt_override.as_deref())
            .unwrap_or(BUILT_IN_SYSTEM_TEMPLATE_NAME);
        self.system_prompt_templates
            .get(template_name)
            .or_else(|| {
                self.system_prompt_templates
                    .get(BUILT_IN_SYSTEM_TEMPLATE_NAME)
            })
            .map(String::as_str)
            .unwrap_or("")
    }

    #[cfg(test)]
    #[expect(dead_code)]
    fn gather_prompt_fragments(&self) -> Vec<PromptFragment> {
        self.gather_prompt_fragments_for_role(&self.selected_role)
    }

    #[cfg(test)]
    fn gather_prompt_fragments_for_role(&self, role_name: &str) -> Vec<PromptFragment> {
        let (fragments, tool_fragments) = self.gather_sourced_prompt_fragment_groups(role_name);
        sorted_prompt_fragments(fragments.into_iter().chain(tool_fragments))
    }

    fn gather_prompt_fragment_groups_for_role(
        &self,
        role_name: &str,
    ) -> (Vec<PromptFragment>, Vec<PromptFragment>) {
        let (fragments, tool_fragments) = self.gather_sourced_prompt_fragment_groups(role_name);
        (
            sorted_prompt_fragments(fragments),
            sorted_prompt_fragments(tool_fragments),
        )
    }

    fn gather_sourced_prompt_fragment_groups(
        &self,
        role_name: &str,
    ) -> (Vec<SourcedPromptFragment>, Vec<SourcedPromptFragment>) {
        let mut fragments: Vec<_> = self
            .extension_prompt_fragments
            .iter()
            .flat_map(|(connection_id, fragments)| {
                fragments
                    .values()
                    .map(move |fragment| SourcedPromptFragment {
                        source: PromptFragmentSource::Extension {
                            connection_id: connection_id.clone(),
                        },
                        fragment: fragment.clone(),
                    })
            })
            .collect();
        if let Some(role) = self.available_roles.get(role_name) {
            fragments.extend(
                role.prompt_fragments
                    .iter()
                    .map(|fragment| SourcedPromptFragment {
                        source: PromptFragmentSource::RoleConfig {
                            role_name: role_name.to_owned(),
                        },
                        fragment: PromptFragment::new(
                            fragment.name.clone(),
                            fragment.priority,
                            fragment.text.clone(),
                        ),
                    }),
            );
        }
        let tool_fragments = self
            .registry
            .all_tool_providers()
            .into_iter()
            .filter(|provider| self.is_tool_enabled_for_role(&provider.tool, role_name))
            .filter_map(|provider| {
                provider
                    .prompt_fragment
                    .as_ref()
                    .map(|fragment| SourcedPromptFragment {
                        source: PromptFragmentSource::Tool {
                            connection_id: provider.connection_id.clone(),
                        },
                        fragment: fragment.clone(),
                    })
            })
            .collect::<Vec<_>>();
        (fragments, tool_fragments)
    }

    fn gather_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.gather_tool_definitions_for_role(&self.selected_role)
    }

    fn gather_tool_definitions_for_role(&self, role_name: &str) -> Vec<ToolDefinition> {
        self.registry
            .all_tools()
            .into_iter()
            .filter(|spec| self.is_tool_enabled_for_role(spec, role_name))
            .map(|spec| ToolDefinition {
                name: spec.name.clone(),
                model_visible_name: spec.model_visible_name.clone(),
                description: spec.description.clone(),
                tool_type: spec.tool_type,
                parameters: spec.parameters.clone(),
                format: spec.format.clone(),
            })
            .collect()
    }

    fn tool_model_visible_name<'a>(&self, spec: &'a tau_proto::ToolSpec) -> &'a ToolName {
        spec.model_visible_name.as_ref().unwrap_or(&spec.name)
    }

    fn has_registered_tool_name(&self, requested_name: &ToolName) -> bool {
        for spec in self.registry.all_tools() {
            if spec.name == *requested_name || self.tool_model_visible_name(spec) == requested_name
            {
                return true;
            }
        }
        false
    }

    fn tool_call_waits_for_staged_registration(
        &self,
        cid: &ConversationId,
        requested_name: &ToolName,
    ) -> bool {
        let role_name = self.role_name_for_conversation_id(cid);
        if self
            .resolve_enabled_tool_name_for_role(requested_name, &role_name)
            .is_some()
        {
            return false;
        }
        self.extension_activation_staging.values().any(|stage| {
            stage.tool_registrations.iter().any(|registration| {
                self.is_tool_enabled_for_role(&registration.tool, &role_name)
                    && (registration.tool.name == *requested_name
                        || self.tool_model_visible_name(&registration.tool) == requested_name)
            })
        })
    }

    fn resolve_enabled_tool_spec_for_role(
        &self,
        requested_name: &ToolName,
        role_name: &str,
    ) -> Option<&tau_proto::ToolSpec> {
        let mut visible_match: Option<&tau_proto::ToolSpec> = None;
        for spec in self.registry.all_tools() {
            if !self.is_tool_enabled_for_role(spec, role_name) {
                continue;
            }
            if spec.name == *requested_name {
                return Some(spec);
            }
            if self.tool_model_visible_name(spec) == requested_name && visible_match.is_none() {
                visible_match = Some(spec);
            }
        }
        visible_match
    }

    fn resolve_enabled_tool_name_for_role(
        &self,
        requested_name: &ToolName,
        role_name: &str,
    ) -> Option<(ToolName, ToolName)> {
        self.resolve_enabled_tool_spec_for_role(requested_name, role_name)
            .map(|spec| {
                (
                    spec.name.clone(),
                    self.tool_model_visible_name(spec).clone(),
                )
            })
    }

    fn is_tool_enabled_for_role(&self, spec: &tau_proto::ToolSpec, role_name: &str) -> bool {
        #[cfg(any(test, feature = "echo-agent"))]
        if spec.name.as_str() == "echo" {
            return true;
        }

        let Some(role) = self.available_roles.get(role_name) else {
            return spec.enabled_by_default;
        };
        let enabled = match role.tools.as_ref() {
            Some(tools) => tools.iter().any(|name| name == &spec.name),
            None => spec.enabled_by_default,
        } || role.enable_tools.iter().any(|name| name == &spec.name);
        enabled && !role.disable_tools.iter().any(|name| name == &spec.name)
    }

    fn maybe_emit_cache_miss_diagnostic(
        &mut self,
        response: &ProviderResponseFinished,
        previous_input_tokens: Option<u64>,
    ) {
        let Some(context) = self
            .prompt_cache_diagnostics
            .remove(&response.session_prompt_id)
        else {
            return;
        };
        let Some(previous_response) = context.previous_response else {
            return;
        };
        let (Some(input_tokens), Some(cached_tokens), Some(previous_input_tokens)) = (
            response
                .usage
                .as_ref()
                .map(|usage| usage.prompt_sent_tokens),
            response
                .usage
                .as_ref()
                .map(|usage| usage.prompt_cached_tokens),
            previous_input_tokens,
        ) else {
            return;
        };
        const PROMPT_CACHE_CHUNK_TOKENS: u64 = 512;

        let cacheable_input_tokens = previous_input_tokens.min(input_tokens);
        let cacheable_input_tokens =
            cacheable_input_tokens / PROMPT_CACHE_CHUNK_TOKENS * PROMPT_CACHE_CHUNK_TOKENS;
        if cacheable_input_tokens == 0 {
            return;
        }
        // Corrected efficiency ignores newly-added prompt content by
        // comparing cached tokens to the smaller of the previous and
        // current input totals, rounded down to the provider cache
        // chunk size. Emit only clear misses; healthy chained turns
        // should be close to 1.0 here.
        if cacheable_input_tokens < cached_tokens.saturating_mul(2) {
            return;
        }
        self.publish_event(
            None,
            Event::ProviderCacheMissDiagnostic(ProviderCacheMissDiagnostic {
                session_prompt_id: response.session_prompt_id.clone(),
                model: context.model,
                previous_response_id: previous_response.provider_response_id,
                previous_response_message_index: previous_response.next_item_index,
                message_prefix_count: None,
                originator: context.originator,
                tool_choice: context.tool_choice,
                prompt_cache_key: None,
                ws_pool_delta: response.ws_pool_delta,
                request_body_fingerprint: hex_bytes(&context.request_fingerprint),
                input_tokens,
                cached_tokens,
                previous_input_tokens,
                cacheable_input_tokens,
                corrected_cache_efficiency: cached_tokens as f32 / cacheable_input_tokens as f32,
            }),
        );
    }

    fn finish_pending_compaction(
        &mut self,
        summary_cid: ConversationId,
        mut response: ProviderResponseFinished,
        source: Option<&str>,
    ) -> Result<(), HarnessError> {
        let requested_tool_calls = response_requests_tool_calls(&response);
        let replacement_window = compaction_items_from_output_items(&response.output_items);
        let text = assistant_text_from_output_items(&response.output_items);
        let Some(pending) = self.pending_compactions.remove(&summary_cid) else {
            return Ok(());
        };

        response.target_agent_id = pending.target_agent_id.clone();
        self.publish_for_conversation_from(
            &summary_cid,
            source,
            Event::ProviderResponseFinished(response.clone()),
        );
        self.prompt_conversations
            .remove(response.session_prompt_id.as_str());
        self.pending_provider_prompts
            .remove(&response.session_prompt_id);
        self.prompt_models.remove(&response.session_prompt_id);
        self.prompt_fingerprints.remove(&response.session_prompt_id);
        self.prompt_cache_diagnostics
            .remove(&response.session_prompt_id);
        self.completed_prompts
            .insert(response.session_prompt_id.clone());
        self.remove_conversation(&summary_cid);

        let Some(target_conv) = self.conversations.get_mut(&pending.target_cid) else {
            self.publish_event(
                None,
                Event::SessionCompactionFinished(tau_proto::SessionCompactionFinished {
                    session_id: pending.session_id,
                    target_agent_id: pending.target_agent_id,
                    originator: pending.originator,
                    original_input_tokens: pending.original_input_tokens,
                    compacted_input_tokens: None,
                    outcome: tau_proto::SessionCompactionOutcome::Failed,
                    message: Some("target conversation no longer exists".to_owned()),
                }),
            );
            return Ok(());
        };
        target_conv.turn_state = ConversationTurnState::Idle;
        target_conv.chain_anchor = None;
        target_conv.last_prompt_id = None;
        target_conv.context_input_tokens = None;
        target_conv.context_percent_used = None;
        target_conv.result_dedup = crate::dedup::ResultDedupMap::new();

        if pending.target_cid == self.default_conversation_id {
            self.update_context_usage(None, None);
        }

        let (outcome, message, compacted_input_tokens) = if requested_tool_calls {
            (
                tau_proto::SessionCompactionOutcome::Failed,
                Some("tool call attempted".to_owned()),
                None,
            )
        } else if !replacement_window.is_empty() {
            let compacted_input_tokens = response
                .usage
                .as_ref()
                .and_then(|usage| {
                    (usage.response_received_tokens > 0).then_some(usage.response_received_tokens)
                })
                .or_else(|| estimate_compacted_input_tokens(&replacement_window));
            self.publish_for_conversation(
                &pending.target_cid,
                Event::SessionCompacted(tau_proto::SessionCompacted {
                    session_id: pending.session_id.clone(),
                    target_agent_id: pending.target_agent_id.clone(),
                    originator: pending.originator.clone(),
                    original_input_tokens: pending.original_input_tokens,
                    compacted_input_tokens,
                    replacement_window: replacement_window.clone(),
                }),
            );
            (
                tau_proto::SessionCompactionOutcome::Succeeded,
                None,
                compacted_input_tokens,
            )
        } else {
            let message = text
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty() && *text != "Conversation compacted.")
                .map(|text| text.strip_prefix("LLM error: ").unwrap_or(text).to_owned())
                .unwrap_or_else(|| "no compacted window".to_owned());
            (
                tau_proto::SessionCompactionOutcome::Failed,
                Some(message),
                None,
            )
        };
        self.publish_event(
            None,
            Event::SessionCompactionFinished(tau_proto::SessionCompactionFinished {
                session_id: pending.session_id.clone(),
                target_agent_id: pending.target_agent_id.clone(),
                originator: pending.originator.clone(),
                original_input_tokens: pending.original_input_tokens,
                compacted_input_tokens,
                outcome,
                message,
            }),
        );

        let folded_background_prompts =
            self.fold_queued_background_completion_prompts(&pending.target_cid);
        match pending.resume {
            PendingCompactionResume::UserPrompt(text) => {
                for restore_prompt in
                    self.take_pending_restore_prompts_for_user_prompt(&pending.target_cid)
                {
                    self.publish_pending_prompt_for_conversation(
                        &pending.target_cid,
                        restore_prompt,
                    )?;
                }
                self.publish_pending_prompt_for_conversation(
                    &pending.target_cid,
                    PendingPrompt::user(text),
                )?;
                self.dispatch_prompt_after_publish_idle(&pending.target_cid);
                Ok(())
            }
            PendingCompactionResume::FollowupTurn => {
                self.dispatch_prompt_after_publish_idle(&pending.target_cid);
                Ok(())
            }
            PendingCompactionResume::None => {
                if folded_background_prompts {
                    self.dispatch_prompt_after_publish_idle(&pending.target_cid);
                }
                // Manual compaction has no built-in follow-up prompt, but a
                // user may have queued one while the target conversation was in
                // `Compacting`. Always give the queue a chance to drain after
                // restoring the target to `Idle`; otherwise the queued prompt
                // can remain parked until some unrelated state change occurs.
                self.try_advance_queue();
                Ok(())
            }
        }
    }

    #[cfg(test)]
    fn handle_provider_response_finished(
        &mut self,
        response: ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        self.handle_provider_response_finished_from(None, response)
    }

    fn handle_provider_response_finished_from(
        &mut self,
        source: Option<&str>,
        mut response: ProviderResponseFinished,
    ) -> Result<(), HarnessError> {
        let mut tool_calls = tool_calls_from_output_items(&response.output_items);
        let mut requested_tool_calls = response_requests_tool_calls(&response);
        let assistant_text = assistant_text_from_output_items(&response.output_items);
        let input_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_sent_tokens);
        let cached_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.prompt_cached_tokens);
        let output_tokens = response
            .usage
            .as_ref()
            .map(|usage| usage.response_received_tokens);
        if self.canceled_prompts.remove(&response.session_prompt_id) {
            self.prompt_conversations
                .remove(response.session_prompt_id.as_str());
            self.pending_provider_prompts
                .remove(&response.session_prompt_id);
            self.prompt_models.remove(&response.session_prompt_id);
            self.prompt_fingerprints.remove(&response.session_prompt_id);
            self.prompt_cache_diagnostics
                .remove(&response.session_prompt_id);
            return Ok(());
        }
        let response_cid = self.conversation_for_prompt(&response.session_prompt_id);
        if (input_tokens.is_some() || cached_tokens.is_some())
            && response_cid.as_ref() == Some(&self.default_conversation_id)
        {
            self.update_context_usage(input_tokens, cached_tokens);
        }
        // Per-conversation usage: separate from the global tracker
        // because side conversations shouldn't clobber the user's
        // status bar, but the harness still needs their context %
        // to surface via `DelegateProgress`.
        if let Some(cid) = response_cid.as_ref() {
            let previous_input_tokens = self
                .conversations
                .get(cid)
                .and_then(|conv| conv.context_input_tokens);
            self.maybe_emit_cache_miss_diagnostic(&response, previous_input_tokens);
            let usage_model = self.prompt_models.get(&response.session_prompt_id).cloned();
            self.update_conversation_context_usage(cid, usage_model.as_ref(), input_tokens);
            self.emit_delegate_progress(cid);
        }
        // Dedupe: under at-least-once delivery the agent may resend a
        // finished-response after a reconnect. The first delivery
        // removed the entry from `prompt_conversations`; later ones
        // must be ignored rather than fall through to the "default"
        // session fallback, which would silently misroute the
        // duplicate.
        let Some(cid) = response_cid else {
            self.emit_info(&format!(
                "discarding duplicate agent response for session_prompt_id={}",
                response.session_prompt_id
            ));
            return Ok(());
        };
        response.target_agent_id = self.target_agent_id_for_conversation(&cid);

        let stale_behind_newer_prompt = self.conversations.get(&cid).is_some_and(|conv| {
            conv.last_prompt_id
                .as_ref()
                .is_some_and(|last| last != &response.session_prompt_id)
                || conv
                    .in_flight_prompt
                    .as_ref()
                    .is_some_and(|in_flight| in_flight != &response.session_prompt_id)
        });
        if stale_behind_newer_prompt {
            if let Some((session_id, originator)) = self
                .conversations
                .get(&cid)
                .map(|conv| (conv.session_id.clone(), conv.originator.clone()))
            {
                self.publish_prompt_terminated(
                    session_id,
                    response.session_prompt_id.clone(),
                    SessionPromptTerminationReason::Stale,
                    originator,
                );
            }
            self.emit_info(&format!(
                "discarding stale agent response for session_prompt_id={}",
                response.session_prompt_id
            ));
            self.prompt_conversations
                .remove(response.session_prompt_id.as_str());
            self.pending_provider_prompts
                .remove(&response.session_prompt_id);
            self.prompt_models.remove(&response.session_prompt_id);
            self.prompt_fingerprints.remove(&response.session_prompt_id);
            self.prompt_cache_diagnostics
                .remove(&response.session_prompt_id);
            self.completed_prompts
                .insert(response.session_prompt_id.clone());
            return Ok(());
        }
        // Save the model that ran this turn before the
        // `prompt_models` entry is consumed below — we'll need it
        // again to anchor the stateful-chain state, and re-reading
        // `selected_model` later would lie if the user switched
        // models mid-turn.
        let turn_model = self.prompt_models.remove(&response.session_prompt_id);
        let turn_fingerprint = self.prompt_fingerprints.remove(&response.session_prompt_id);
        if let Some(ref model) = turn_model {
            let sent_tokens = input_tokens.unwrap_or(0);
            let cached_tokens = cached_tokens.unwrap_or(0);
            let received_tokens = output_tokens.unwrap_or(0);
            self.current_session_state
                .token_usage
                .add_sent(model, sent_tokens, cached_tokens);
            self.current_session_state
                .token_usage
                .add_received(model, received_tokens);
            response.usage = Some(ProviderTokenUsage {
                model: Some(model.clone()),
                prompt_sent_tokens: sent_tokens,
                prompt_cached_tokens: cached_tokens,
                response_received_tokens: received_tokens,
                stats: self.current_session_state.token_usage.clone(),
            });
        }
        // Stamp the live-header `display` descriptor on each tool
        // call so renderers don't need per-tool string knowledge.
        for call in &mut tool_calls {
            if call.display.is_some() {
                continue;
            }
            call.display = build_tool_args_display(call.name.as_str(), &call.arguments);
        }
        if self.pending_compactions.contains_key(&cid) {
            return self.finish_pending_compaction(cid, response, source);
        }
        if requested_tool_calls && tool_calls.is_empty() {
            self.emit_info(&format!(
                "agent response {} reported tool calls but contained none; treating it as end_turn",
                response.session_prompt_id
            ));
            requested_tool_calls = false;
        }
        if requested_tool_calls
            && let Some(call) = tool_calls.iter().find(|call| call.id.as_str().is_empty())
        {
            return Err(HarnessError::Participant(format!(
                "agent response {} contained tool call {} with empty call_id",
                response.session_prompt_id, call.name
            )));
        }
        let is_non_tool_ext_query = self.conversations.get(&cid).is_some_and(|conv| {
            matches!(
                conv.originator,
                tau_proto::PromptOriginator::Extension { .. }
            ) && conv.parent_tool_call_id.is_none()
        });

        let mut normalized_calls: Vec<(
            AgentToolCall,
            tau_proto::ToolExecutionMode,
            BackgroundSupport,
        )> = Vec::new();
        if requested_tool_calls {
            normalized_calls = tool_calls
                .iter()
                .map(|call| {
                    let call = call.clone();
                    let mode = self.resolve_tool_execution_mode_for_call(&call);
                    let background_support =
                        self.resolve_tool_background_support(call.name.as_str());
                    (call, mode, background_support)
                })
                .collect();
            let mut normalized_calls_iter = normalized_calls.iter();
            response.output_items = response
                .output_items
                .into_iter()
                .map(|item| match item {
                    ContextItem::ToolCall(_) => {
                        let (call, _, _) = normalized_calls_iter
                            .next()
                            .expect("tool-call normalization count should match output items");
                        ContextItem::ToolCall(ToolCallItem {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            tool_type: call.tool_type,
                            arguments: call.arguments.clone(),
                        })
                    }
                    item => item,
                })
                .collect();
            tool_calls = normalized_calls
                .iter()
                .map(|(call, _, _)| call.clone())
                .collect();
        }

        // Publish via the owning conversation's branch — when text is
        // present the SessionTree fold appends an `AgentMessage` as a
        // child of `tree.head`, so an unsnapped publish would land on
        // whichever branch happened to be at `tree.head` (e.g. after
        // a sibling side conv's teardown ran `snap_to_default`).
        // `publish_for_conversation` snaps and updates `c.head`.
        self.publish_for_conversation_from(
            &cid,
            source,
            Event::ProviderResponseFinished(response.clone()),
        );
        self.prompt_conversations
            .remove(response.session_prompt_id.as_str());
        self.pending_provider_prompts
            .remove(&response.session_prompt_id);
        // Stateful-chain anchor: set only when the agent supplied a
        // `response_id` (i.e. the upstream backend exposed one — the
        // Responses API does, Chat Completions doesn't). The anchor
        // pins this conversation's current head + assembled message
        // count so the next `send_prompt_to_agent_for` can send a
        // delta instead of replaying the full transcript.
        if let (Some(response_id), Some(model), Some(request_fingerprint), Some(backend)) = (
            response.provider_response_id.clone(),
            turn_model,
            turn_fingerprint,
            response.backend.clone(),
        ) {
            let (conv_head, conv_session) = self
                .conversations
                .get(&cid)
                .map(|c| (c.head, c.session_id.clone()))
                .unzip();
            let message_count = conv_session
                .as_ref()
                .and_then(|sid| self.store.session(sid.as_str()))
                .map(|tree| assemble_conversation_from(tree, conv_head.flatten()).len())
                .unwrap_or(0);
            if let Some(conv) = self.conversations.get_mut(&cid) {
                conv.chain_anchor = Some(crate::conversation::ChainAnchor {
                    response_id,
                    head: conv_head.flatten(),
                    model,
                    message_count,
                    backend,
                    request_fingerprint: request_fingerprint.digest,
                    request_fingerprint_parts: request_fingerprint.parts,
                });
            }
        }
        if let Some(conv) = self.conversations.get_mut(&cid) {
            conv.in_flight_prompt = None;
            conv.turn_state = ConversationTurnState::Idle;
        }
        self.completed_prompts
            .insert(response.session_prompt_id.clone());

        // Side-conversation handling: if this prompt originated from
        // an extension via StartAgentRequest, route the final text back
        // to the requesting extension as StartAgentResult and
        // tear down the side conversation. The harness routes tool
        // calls per-conversation, so any in-flight
        // pending_tool_invocations entries for this side conversation
        // have already been emitted into the bus and will complete
        // normally even after teardown.
        if let tau_proto::PromptOriginator::Extension {
            ref name,
            ref query_id,
        } = response.originator
            && (!requested_tool_calls || is_non_tool_ext_query)
        {
            if self.has_pending_agent_message_prompt(&cid) {
                self.fold_pending_prompts_as_steered(&cid);
                if !self.maybe_start_auto_compaction_for_followup(&cid) {
                    self.dispatch_prompt_after_publish_idle(&cid);
                }
                return Ok(());
            }

            let source = self
                .conversations
                .get(&cid)
                .and_then(|c| c.source_connection.clone());
            let error = if is_non_tool_ext_query && requested_tool_calls {
                Some(format!(
                    "non-tool extension query attempted to call {} tool(s); refusing to execute",
                    tool_calls.len()
                ))
            } else {
                None
            };
            let result = tau_proto::StartAgentResult {
                query_id: query_id.clone(),
                text: assistant_text.clone().unwrap_or_default(),
                error,
            };
            if let Some(source) = source {
                if source.as_str() == HARNESS_CONNECTION_ID {
                    self.publish_event(
                        Some(HARNESS_CONNECTION_ID),
                        Event::StartAgentResult(result),
                    );
                } else {
                    let _ = self.bus.send_to(
                        source.as_str(),
                        None,
                        Frame::Event(Event::StartAgentResult(result)),
                    );
                }
            } else {
                // Should never happen — `source_connection` is set in
                // `handle_start_agent_request` when the conversation is
                // spawned. Surface it via `harness.info` rather than
                // silently dropping so a future regression is visible.
                self.emit_info(&format!(
                    "start-agent-request result for `{}` (extension `{}`) had no source connection — \
                         dropping",
                    query_id, name
                ));
            }
            // Snap the tree head back to the default conversation's
            // local head so the user's next interactive prompt
            // continues on the main branch instead of the side branch.
            self.snap_to_default_conversation();
            let completed_agent_id = self
                .conversations
                .get(&cid)
                .and_then(|conv| conv.agent_id.clone());
            let keep_tool_backed_conversation = self
                .conversations
                .get(&cid)
                .is_some_and(|conv| conv.parent_tool_call_id.is_some());
            let should_auto_suspend_delegate = keep_tool_backed_conversation
                && completed_agent_id.as_deref().is_some_and(|agent_id| {
                    self.agent_states.get(agent_id).copied() == Some(AgentState::ActiveDelegated)
                });
            // Release before removing or detaching the side conversation so
            // queued descendants can still resolve their parent conversation
            // while starting. Active descendants keep their own copied state.
            self.release_start_agent_request(&cid);
            if keep_tool_backed_conversation {
                if should_auto_suspend_delegate
                    && let Some(agent_id) = completed_agent_id.as_deref()
                {
                    self.set_agent_state(agent_id, AgentState::Suspended);
                }
                self.detach_completed_tool_backed_start_agent(&cid);
            } else {
                self.transfer_background_completion_target_before_teardown(&cid);
                self.remove_conversation(&cid);
            }
            self.try_advance_queue();
            return Ok(());
        }

        if requested_tool_calls {
            // Tool calls to execute — agent stays busy. After all
            // tools complete, maybe_complete_agent_turn drains any
            // prompts queued via `pending_prompts` (publishing one
            // `SessionPromptSteered` each, which folds them as
            // `UserMessage` entries onto this conversation's branch)
            // and sends a new prompt with the results plus those
            // steering messages.
            // Normalize empty call_ids to a synthetic one. Models
            // sometimes emit hallucinated tool calls with both a
            // missing name *and* a missing id; an empty id would
            // collide with itself in `in_flight_tool_execution_modes` /
            // `pending_tool_sessions`, and would later render into
            // conversation history as an empty `call_id` which the
            // OpenAI Responses API rejects with
            // `input[N].call_id: empty string`. Fix it at the boundary.
            let remaining_calls: Vec<ToolCallId> = normalized_calls
                .iter()
                .map(|(call, _, _)| call.id.clone())
                .collect();
            for (call, _, _) in &normalized_calls {
                self.pending_tools.insert(
                    call.id.clone(),
                    PendingTool {
                        name: call.name.clone(),
                        internal_name: call.name.clone(),
                        tool_type: call.tool_type,
                    },
                );
            }
            if let Some(conv) = self.conversations.get_mut(&cid) {
                conv.turn_state = ConversationTurnState::ToolsRunning { remaining_calls };
            }
            if self
                .conversations
                .get(&cid)
                .is_some_and(|conv| conv.pending_cancel.is_some())
            {
                self.apply_pending_cancel_for_conversation(&cid);
                return Ok(());
            }
            // Enqueue in the order the agent emitted them. Dispatch is done by
            // `drain_pending_tool_invocations`, which respects the execution
            // mode ordering rules.
            for (call, mode, background_support) in normalized_calls {
                self.tool_turn
                    .push(cid.clone(), call, mode, background_support);
            }
            self.drain_pending_tool_invocations()?;
        } else {
            // No tool calls — this conversation's turn is done. Drain
            // any queued prompts (on this or other conversations) that
            // are now eligible to dispatch.
            self.try_advance_queue();
        }

        Ok(())
    }

    /// Update one conversation's `context_input_tokens` /
    /// `context_percent_used` from a finished agent response. Mirrors
    /// `update_context_usage` but scoped to a single conversation —
    /// the global tracker is intentionally only fed by the user's
    /// default conversation so the status bar stays stable while side
    /// conversations run.
    fn update_conversation_context_usage(
        &mut self,
        cid: &ConversationId,
        model: Option<&ModelId>,
        input_tokens: Option<u64>,
    ) {
        let context_window =
            model.and_then(|m| context_window_for_model(&self.provider_model_info, m));
        let percent_used = match (context_window, input_tokens) {
            (Some(w), Some(tokens)) => Some(context_percent_used(tokens, w)),
            _ => None,
        };
        if let Some(conv) = self.conversations.get_mut(cid) {
            if input_tokens.is_some() {
                conv.context_input_tokens = input_tokens;
            }
            if percent_used.is_some() {
                conv.context_percent_used = percent_used;
            }
        }
    }

    fn update_context_usage(&mut self, input_tokens: Option<u64>, cached_tokens: Option<u64>) {
        let context_window = self
            .selected_model
            .as_ref()
            .and_then(|m| context_window_for_model(&self.provider_model_info, m));
        let percent_used = match (context_window, input_tokens) {
            (Some(w), Some(tokens)) => Some(context_percent_used(tokens, w)),
            _ => None,
        };
        if self.current_session_state.context_input_tokens == input_tokens
            && self.current_session_state.context_cached_tokens == cached_tokens
            && self.current_session_state.context_percent_used == percent_used
        {
            return;
        }
        self.current_session_state.context_input_tokens = input_tokens;
        self.current_session_state.context_cached_tokens = cached_tokens;
        self.current_session_state.context_percent_used = percent_used;
        self.publish_event(
            None,
            Event::HarnessContextUsageChanged(HarnessContextUsageChanged {
                input_tokens,
                cached_tokens,
                percent_used,
            }),
        );
    }

    /// True iff every configured extension has either reached `Ready`
    /// or dropped permanently.
    ///
    /// `Disconnected` counts as "no longer blocking": a dead tool extension
    /// may be on its way to being respawned, but the old connection is gone and
    /// should not wedge fresh prompt dispatch. Provider disconnects are handled
    /// as fatal by the event loop before this predicate matters for new work.
    pub(crate) fn extensions_all_ready(&self) -> bool {
        self.extensions.values().all(|e| {
            matches!(
                e.state,
                ExtensionState::Ready | ExtensionState::Disconnected
            )
        })
    }

    /// Update an extension's lifecycle state, looked up by connection id.
    /// No-op if no entry matches (e.g. for socket clients).
    fn set_extension_state(&mut self, connection_id: &str, new_state: ExtensionState) {
        if let Some(entry) = self.extensions.get_mut(connection_id) {
            entry.state = new_state;
        }
    }

    /// Returns the execution mode of a tool name.
    ///
    /// Falls back to `Exclusive` for unknown tools so an unregistered
    /// name does not accidentally parallelize.
    fn resolve_tool_execution_mode(&self, name: &str) -> tau_proto::ToolExecutionMode {
        self.registry
            .resolve_provider(name)
            .map(|provider| provider.tool.execution_mode)
            .unwrap_or(tau_proto::ToolExecutionMode::Exclusive)
    }

    /// Returns the effective foreground/background support for a tool name.
    /// Missing registration metadata uses the protocol default of
    /// `MinForegroundSeconds(5)`.
    fn resolve_tool_background_support(&self, name: &str) -> BackgroundSupport {
        self.registry
            .resolve_provider(name)
            .and_then(|provider| provider.tool.background_support)
            .unwrap_or_else(BackgroundSupport::default_effective)
    }

    /// Same as [`resolve_tool_execution_mode`] for a concrete tool call.
    ///
    /// `delegate` registers as `Shared` so multiple sub-agent requests can be
    /// handed to the extension from one parent turn. The delegate call's
    /// `execution_mode` argument belongs to the emitted `StartAgentRequest`,
    /// not to this parent-conversation tool invocation; per-delegation
    /// exclusivity is enforced later by the harness-owned
    /// `StartAgentRequest` scheduler.
    fn resolve_tool_execution_mode_for_call(
        &self,
        call: &AgentToolCall,
    ) -> tau_proto::ToolExecutionMode {
        self.resolve_tool_execution_mode(call.name.as_str())
    }

    /// Drain scheduler-selected tool invocations into harness side effects.
    fn drain_pending_tool_invocations(&mut self) -> Result<(), HarnessError> {
        while let Some(next) = self.tool_turn.next_dispatchable().cloned() {
            if self.tool_call_waits_for_staged_registration(
                &next.conversation_id,
                &next.invocation.name,
            ) {
                break;
            }
            let Some((
                PendingToolInvocation {
                    conversation_id,
                    invocation,
                    execution_mode: _,
                    background_support: _,
                },
                foreground_action,
            )) = self.tool_turn.pop_dispatchable(Instant::now())
            else {
                break;
            };
            let call_id = invocation.id.clone();
            // If dispatch fails synchronously, roll back the in-flight
            // entry so a retry or clean-up is not wedged on a phantom
            // slot.
            if let Err(error) = self.execute_agent_tool_call(&conversation_id, &invocation) {
                self.tool_turn.rollback_dispatch(&call_id);
                return Err(error);
            }
            self.apply_foreground_action(foreground_action);
        }
        Ok(())
    }

    fn apply_foreground_action(&mut self, action: ForegroundAction) {
        match action {
            ForegroundAction::None => {}
            ForegroundAction::Background { call_id } => {
                if self.tool_turn.mark_backgrounded(&call_id) {
                    self.publish_synthetic_background_result(&call_id);
                }
                self.on_tool_call_foreground_complete(call_id.as_str());
            }
        }
    }

    fn publish_synthetic_background_result(&mut self, call_id: &ToolCallId) {
        self.publish_synthetic_background_result_inner(call_id, None);
    }

    pub(crate) fn publish_internal_background_placeholder(
        &mut self,
        call_id: &ToolCallId,
        result: CborValue,
    ) {
        let Some(cid) = self.tool_conversations.get(call_id).cloned() else {
            return;
        };
        let Some(tool) = self.pending_tools.get(call_id).cloned() else {
            return;
        };
        let result = ToolResult {
            call_id: call_id.clone(),
            tool_name: tool.name,
            tool_type: tool.tool_type,
            result,
            kind: ToolResultKind::BackgroundPlaceholder,
            display: None,
            originator: PromptOriginator::User,
        };
        self.publish_for_conversation(&cid, Event::ProviderToolResult(result.clone()));
        self.record_wait_tool_result(result);
    }

    fn publish_synthetic_background_result_inner(
        &mut self,
        call_id: &ToolCallId,
        agent_ids: Option<(&str, &str)>,
    ) {
        let Some(cid) = self.tool_conversations.get(call_id).cloned() else {
            return;
        };
        let Some(tool) = self.pending_tools.get(call_id).cloned() else {
            return;
        };
        let agent_id_headers = agent_ids
            .map(|(self_agent_id, sub_agent_id)| {
                format!("self_agent_id: {self_agent_id}\nsub_agent_id: {sub_agent_id}\n")
            })
            .unwrap_or_default();
        let content = format!(
            "{}: true\n{agent_id_headers}\nTool call `{call_id}` is running in the background.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        );
        let result = ToolResult {
            call_id: call_id.clone(),
            tool_name: tool.name,
            tool_type: tool.tool_type,
            result: CborValue::Text(content),
            kind: ToolResultKind::BackgroundPlaceholder,
            display: None,
            originator: PromptOriginator::User,
        };
        self.publish_for_conversation(&cid, Event::ProviderToolResult(result.clone()));
        self.record_wait_tool_result(result);
    }

    fn process_background_deadlines(&mut self) {
        for call_id in self.tool_turn.background_due(Instant::now()) {
            self.publish_synthetic_background_result(&call_id);
            self.on_tool_call_foreground_complete(call_id.as_str());
        }
    }

    pub(crate) fn on_tool_call_foreground_complete(&mut self, call_id: &str) {
        let owner = self.tool_conversations.get(call_id).cloned();
        if let Some(cid) = owner {
            self.emit_delegate_progress(&cid);
        }
        self.drain_pending_tool_invocations_or_report();
        self.maybe_complete_agent_turn(call_id);
        self.try_advance_queue();
    }

    fn drain_pending_tool_invocations_or_report(&mut self) {
        if let Err(error) = self.drain_pending_tool_invocations() {
            self.emit_info(&format!("queued tool dispatch failed: {error}"));
        }
    }

    fn handle_background_tool_result(&mut self, source_id: &str, mut result: ToolResult) {
        let Some(cid) = self.tool_conversations.get(&result.call_id).cloned() else {
            return;
        };
        let call_id = result.call_id.clone();
        if let Some(tool) = self.pending_tools.get(&result.call_id) {
            result.tool_name = tool.name.clone();
            result.tool_type = tool.tool_type;
        }
        self.finish_tool_call_runtime_state(call_id.as_str());
        let background = ToolBackgroundResult {
            call_id: result.call_id,
            tool_name: result.tool_name,
            tool_type: result.tool_type,
            result: result.result,
            display: result.display,
            originator: result.originator,
        };
        self.publish_for_conversation_from(
            &cid,
            Some(source_id),
            Event::ToolBackgroundResult(background.clone()),
        );
        self.record_wait_background_result(background);
        self.background_completion_targets
            .insert(call_id.clone(), cid.clone());
        self.queue_background_completion_prompt(&cid, &call_id);
        // Keep the completion prompt queued before draining. If an unblocked
        // queued call closes the tool round, `maybe_complete_agent_turn` can
        // fold this background notification into that follow-up prompt.
        self.drain_pending_tool_invocations_or_report();
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn handle_background_tool_error(&mut self, source: Option<&str>, error: ToolError) {
        self.handle_background_tool_error_inner(
            source,
            error,
            BackgroundCompletionPromptMode::QueueAndAdvance,
        );
    }

    fn handle_background_tool_cancelled(&mut self, source_id: &str, cancelled: ToolCancelled) {
        let call_id = cancelled.call_id.clone();
        let owner = self.finish_tool_call_runtime_state(call_id.as_str());
        self.record_wait_tool_cancelled(&std::collections::HashSet::from([call_id.clone()]));
        if let Some(cid) = owner {
            self.publish_for_conversation_from(
                &cid,
                Some(source_id),
                Event::ToolCancelled(cancelled),
            );
        }
        self.drain_pending_tool_invocations_or_report();
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn handle_background_tool_error_without_advancing(
        &mut self,
        source: Option<&str>,
        error: ToolError,
    ) {
        self.handle_background_tool_error_inner(
            source,
            error,
            BackgroundCompletionPromptMode::QueueOnly,
        );
    }

    fn handle_background_tool_error_inner(
        &mut self,
        source: Option<&str>,
        mut error: ToolError,
        completion_prompt_mode: BackgroundCompletionPromptMode,
    ) {
        let Some(cid) = self.tool_conversations.get(&error.call_id).cloned() else {
            return;
        };
        let call_id = error.call_id.clone();
        if let Some(tool) = self.pending_tools.get(&error.call_id) {
            error.tool_name = tool.name.clone();
            error.tool_type = tool.tool_type;
        }
        self.tool_turn.mark_complete(&call_id);
        if let Some(conv) = self.conversations.get_mut(&cid) {
            conv.tools_in_flight = conv.tools_in_flight.saturating_sub(1);
        }
        self.emit_delegate_progress(&cid);
        let background = ToolBackgroundError {
            call_id: error.call_id,
            tool_name: error.tool_name,
            tool_type: error.tool_type,
            message: error.message,
            details: error.details,
            display: error.display,
            originator: error.originator,
        };
        self.publish_terminal_background_error(&cid, source, background);
        match completion_prompt_mode {
            BackgroundCompletionPromptMode::QueueAndAdvance => {
                self.background_completion_targets
                    .insert(call_id.clone(), cid.clone());
                self.queue_background_completion_prompt(&cid, &call_id);
                // Keep the completion prompt queued before draining. If an unblocked
                // queued call closes the tool round, `maybe_complete_agent_turn` can
                // fold this background notification into that follow-up prompt.
                self.drain_pending_tool_invocations_or_report();
            }
            BackgroundCompletionPromptMode::QueueOnly => {
                self.background_completion_targets
                    .insert(call_id.clone(), cid.clone());
                self.queue_background_completion_prompt_without_advancing(&cid, &call_id);
            }
        }
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn queue_background_completion_prompt(&mut self, cid: &ConversationId, call_id: &ToolCallId) {
        self.queue_background_completion_prompt_inner(cid, call_id, true);
    }

    fn queue_background_completion_prompt_without_advancing(
        &mut self,
        cid: &ConversationId,
        call_id: &ToolCallId,
    ) {
        self.queue_background_completion_prompt_inner(cid, call_id, false);
    }

    fn queue_background_completion_prompt_inner(
        &mut self,
        cid: &ConversationId,
        call_id: &ToolCallId,
        advance_queue: bool,
    ) {
        if self
            .suppressed_background_completion_prompts
            .contains(call_id)
        {
            return;
        }
        let prompt = background_completion_prompt(call_id);
        if let Some(conv) = self.conversations.get_mut(cid) {
            if conv
                .pending_prompts
                .iter()
                .any(|pending| pending.text == prompt)
            {
                return;
            }
            conv.pending_prompts
                .push_back(PendingPrompt::internal(prompt));
        }
        if advance_queue {
            self.try_advance_queue();
        }
    }

    fn suppress_background_completion_prompt(&mut self, call_id: ToolCallId) {
        self.suppressed_background_completion_prompts
            .insert(call_id.clone());
        let prompt = background_completion_prompt(&call_id);
        for conv in self.conversations.values_mut() {
            conv.pending_prompts
                .retain(|pending| pending.text != prompt);
        }
    }

    fn unsuppress_background_completion_prompt(&mut self, call_id: ToolCallId) {
        self.suppressed_background_completion_prompts
            .remove(&call_id);
        if let Some(cid) = self.background_completion_targets.get(&call_id).cloned() {
            self.queue_background_completion_prompt(&cid, &call_id);
        }
    }

    fn transfer_background_completion_target_before_teardown(&mut self, cid: &ConversationId) {
        let call_ids = self.background_completion_call_ids_for_teardown(cid);
        if call_ids.is_empty() {
            return;
        }
        let Some(target_cid) = self.background_completion_teardown_target(cid) else {
            return;
        };
        for call_id in call_ids {
            if self.tool_conversations.get(&call_id) == Some(cid)
                && self.tool_turn.is_backgrounded(&call_id)
            {
                self.tool_conversations
                    .insert(call_id.clone(), target_cid.clone());
            }
            if self.background_completion_targets.get(&call_id) == Some(cid) {
                self.background_completion_targets
                    .insert(call_id.clone(), target_cid.clone());
            }
            self.transfer_wait_background_owner_before_teardown(&call_id, cid, &target_cid);
            self.transfer_queued_background_completion_prompt(cid, &target_cid, &call_id);
        }
    }

    fn discard_background_completion_target_before_teardown(&mut self, cid: &ConversationId) {
        for call_id in self.background_completion_call_ids_for_teardown(cid) {
            self.suppressed_background_completion_prompts
                .remove(&call_id);
            self.background_completion_targets.remove(&call_id);
            self.discard_wait_background_owner_before_teardown(&call_id, cid);
            self.clear_tool_call_tracking(call_id.as_str());
        }
    }

    fn background_completion_call_ids_for_teardown(
        &self,
        cid: &ConversationId,
    ) -> HashSet<ToolCallId> {
        let mut call_ids: HashSet<ToolCallId> = self
            .tool_turn
            .backgrounded_calls_for(cid)
            .into_iter()
            .collect();
        call_ids.extend(
            self.tool_conversations
                .iter()
                .filter_map(|(call_id, owner)| {
                    (owner == cid && self.tool_turn.is_backgrounded(call_id))
                        .then_some(call_id.clone())
                }),
        );
        call_ids.extend(
            self.background_completion_targets
                .iter()
                .filter_map(|(call_id, owner)| (owner == cid).then_some(call_id.clone())),
        );
        call_ids
    }

    fn transfer_queued_background_completion_prompt(
        &mut self,
        source_cid: &ConversationId,
        target_cid: &ConversationId,
        call_id: &ToolCallId,
    ) {
        let prompt = background_completion_prompt(call_id);
        let removed = self.conversations.get_mut(source_cid).is_some_and(|conv| {
            remove_pending_internal_prompt_text(&mut conv.pending_prompts, &prompt)
        });
        if removed {
            self.queue_background_completion_prompt(target_cid, call_id);
        }
    }

    fn background_completion_teardown_target(
        &self,
        cid: &ConversationId,
    ) -> Option<ConversationId> {
        let conv = self.conversations.get(cid)?;
        if let Some(parent_cid) = &conv.parent_conversation_id
            && parent_cid != cid
            && self.conversations.contains_key(parent_cid)
        {
            return Some(parent_cid.clone());
        }
        if let Some(parent_call_id) = &conv.parent_tool_call_id
            && let Some(parent_cid) = self.tool_conversations.get(parent_call_id)
            && parent_cid != cid
            && self.conversations.contains_key(parent_cid)
        {
            return Some(parent_cid.clone());
        }
        if self.default_conversation_id != *cid
            && self
                .conversations
                .contains_key(&self.default_conversation_id)
        {
            return Some(self.default_conversation_id.clone());
        }
        self.conversations
            .iter()
            .find_map(|(candidate_cid, candidate)| {
                (candidate_cid != cid && candidate.session_id == conv.session_id)
                    .then_some(candidate_cid.clone())
            })
    }

    /// Hook called whenever a tool call has finished (result, error,
    /// synthetic NoProvider error, or inline skill completion). Removes
    /// it from the in-flight set, drains any freshly-eligible queued
    /// calls, and then checks whether the turn is done.
    pub(crate) fn on_tool_call_complete(&mut self, call_id: &str) {
        self.on_tool_call_complete_inner(call_id, true);
    }

    fn on_tool_call_complete_inner(&mut self, call_id: &str, drain_queued: bool) {
        let owner = self.finish_tool_call_runtime_state(call_id);
        if drain_queued {
            self.drain_pending_tool_invocations_or_report();
        }
        if let Some(cid) = owner {
            self.maybe_complete_agent_turn_for(&cid, call_id);
        }
        self.try_advance_queue();
    }

    fn finish_tool_call_runtime_state(&mut self, call_id: &str) -> Option<ConversationId> {
        let owned: ToolCallId = call_id.to_owned().into();
        self.tool_turn.mark_complete(&owned);
        // `tool_conversations` is still populated here: the call
        // sites clear it *after* this function returns. Decrement
        // the conversation's in-flight counter and surface the new
        // state to any UI watching this delegate flow before the
        // mapping is cleared.
        let owner = self.tool_conversations.get(call_id).cloned();
        if let Some(cid) = owner.as_ref()
            && let Some(conv) = self.conversations.get_mut(cid)
        {
            conv.tools_in_flight = conv.tools_in_flight.saturating_sub(1);
        }
        if let Some(cid) = owner.as_ref() {
            self.emit_delegate_progress(cid);
        }
        owner
    }

    /// Bump the per-conversation tool counters for a freshly-started
    /// tool call. Always emits a `DelegateProgress` snapshot when the
    /// conversation is a delegate side conversation (no-op otherwise),
    /// so the UI updates the moment the sub-agent starts a new call
    /// rather than waiting for completion.
    pub(crate) fn bump_tools_started_for(&mut self, cid: &ConversationId) {
        if let Some(conv) = self.conversations.get_mut(cid) {
            conv.tools_in_flight = conv.tools_in_flight.saturating_add(1);
            conv.tools_total = conv.tools_total.saturating_add(1);
        }
        self.emit_delegate_progress(cid);
    }

    fn maybe_complete_agent_turn(&mut self, completed_call_id: &str) {
        let Some(cid) = self.tool_conversations.get(completed_call_id).cloned() else {
            return;
        };
        self.maybe_complete_agent_turn_for(&cid, completed_call_id);
    }

    fn maybe_complete_agent_turn_for(&mut self, cid: &ConversationId, completed_call_id: &str) {
        let should_send = if let Some(conv) = self.conversations.get_mut(cid) {
            if let ConversationTurnState::ToolsRunning { remaining_calls } = &mut conv.turn_state {
                remaining_calls.retain(|id| id.as_str() != completed_call_id);
                if remaining_calls.is_empty() {
                    conv.turn_state = ConversationTurnState::Idle;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if should_send {
            self.fold_pending_prompts_as_steered(cid);
            if self.maybe_start_auto_compaction_for_followup(cid) {
                return;
            }
            // If folding the steered prompts parked any of them in
            // interception (e.g. an extension intercepting
            // `session.prompt_steered`), defer the agent dispatch
            // until the whole publish chain drains. Waiting for only
            // one user-message commit is not enough when several
            // steered prompts are queued behind one interceptor.
            self.dispatch_prompt_after_publish_idle(cid);
        }
    }

    fn has_pending_agent_message_prompt(&self, cid: &ConversationId) -> bool {
        self.conversations.get(cid).is_some_and(|conv| {
            conv.pending_prompts
                .iter()
                .any(PendingPrompt::is_agent_message)
        })
    }

    fn publish_prompts_as_steered(&mut self, cid: &ConversationId, prompts: Vec<PendingPrompt>) {
        let session_id = match self.conversations.get(cid) {
            Some(c) => c.session_id.clone(),
            None => return,
        };
        for prompt in prompts {
            self.publish_for_conversation(
                cid,
                Event::SessionPromptSteered(tau_proto::SessionPromptSteered {
                    session_id: session_id.clone(),
                    text: prompt.text,
                    target_agent_id: self
                        .conversations
                        .get(cid)
                        .and_then(|conv| conv.agent_id.clone()),
                    message_class: prompt.message_class,
                }),
            );
        }
    }

    fn fold_queued_background_completion_prompts(&mut self, cid: &ConversationId) -> bool {
        let completion_texts: HashSet<String> = self
            .background_completion_targets
            .iter()
            .filter(|&(call_id, owner)| {
                owner == cid
                    && !self
                        .suppressed_background_completion_prompts
                        .contains(call_id)
            })
            .map(|(call_id, _)| background_completion_prompt(call_id))
            .collect();
        if completion_texts.is_empty() {
            return false;
        }
        let prompts: Vec<PendingPrompt> = self
            .conversations
            .get_mut(cid)
            .map(|conv| {
                let mut folded = Vec::new();
                let mut retained = VecDeque::with_capacity(conv.pending_prompts.len());
                while let Some(prompt) = conv.pending_prompts.pop_front() {
                    if prompt.is_internal() && completion_texts.contains(&prompt.text) {
                        folded.push(prompt);
                    } else {
                        retained.push_back(prompt);
                    }
                }
                conv.pending_prompts = retained;
                folded
            })
            .unwrap_or_default();
        if prompts.is_empty() {
            return false;
        }
        self.publish_prompts_as_steered(cid, prompts);
        true
    }

    /// Drain any prompts queued on `cid` while the agent was in
    /// flight, and publish a `SessionPromptSteered` event for each. The
    /// folder in `SessionTree::apply_event` appends them as
    /// `UserMessage` entries on this conversation's branch, so the
    /// next-round `SessionPromptCreated` (about to be emitted by the
    /// caller) picks them up alongside the tool results without any
    /// extra wiring on the prompt-assembly side.
    ///
    /// Called from `maybe_complete_agent_turn` only — fresh prompts
    /// arriving on an idle conversation go through
    /// `dispatch_prompt_for_conversation`, which already publishes its
    /// own `UiPromptSubmitted`. Folding here exists specifically to
    /// give queued prompts a chance to ride the next per-round prompt
    /// rather than waiting for the whole turn to terminate.
    fn fold_pending_prompts_as_steered(&mut self, cid: &ConversationId) {
        let mut pending: Vec<PendingPrompt> = self
            .conversations
            .get_mut(cid)
            .map(|c| c.pending_prompts.drain(..).collect())
            .unwrap_or_default();
        if let Some(user_prompt_pos) = pending.iter().position(|prompt| !prompt.is_internal()) {
            let restore_prompts = self.take_pending_restore_prompts_for_user_prompt(cid);
            if !restore_prompts.is_empty() {
                pending.splice(user_prompt_pos..user_prompt_pos, restore_prompts);
            }
        }
        self.publish_prompts_as_steered(cid, pending);
    }

    fn reject_agent_tool_call_before_dispatch(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
        tool_name: ToolName,
        message: String,
    ) {
        let call_id: ToolCallId = call.id.clone();
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.publish_for_conversation(
            cid,
            Event::ProviderToolError(ToolError {
                call_id: call_id.clone(),
                tool_name,
                tool_type: call.tool_type,
                message,
                details: None,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        );
        self.on_tool_call_complete(call_id.as_str());
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn execute_agent_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
    ) -> Result<(), HarnessError> {
        let tool_name = call.name.clone();
        let role_name = self.role_name_for_conversation_id(cid).to_owned();

        let Some(tool_spec) = self.resolve_enabled_tool_spec_for_role(&tool_name, &role_name)
        else {
            let message = if self.has_registered_tool_name(&tool_name) {
                "tool is not enabled for the current role".to_owned()
            } else {
                unavailable_tool_error_message(&tool_name)
            };
            let call_id: ToolCallId = call.id.clone();
            self.tool_conversations.insert(call_id.clone(), cid.clone());
            self.pending_tools.insert(
                call_id.clone(),
                PendingTool {
                    name: tool_name.clone(),
                    internal_name: tool_name.clone(),
                    tool_type: call.tool_type,
                },
            );
            self.bump_tools_started_for(cid);
            self.record_wait_tool_request(&call_id);
            let request = ToolRequest {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                originator: tau_proto::PromptOriginator::User,
            };
            self.publish_for_conversation(cid, Event::ToolRequest(request));
            self.publish_terminal_tool_error(
                Some(cid),
                None,
                ToolError {
                    call_id: call_id.clone(),
                    tool_name,
                    tool_type: call.tool_type,
                    message,
                    details: None,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                },
            );
            self.on_tool_call_complete(call_id.as_str());
            self.clear_tool_call_tracking(call_id.as_str());
            return Ok(());
        };
        let internal_tool_name = tool_spec.name.clone();
        let visible_tool_name = self.tool_model_visible_name(tool_spec).clone();
        if let Err(error) = validate_tool_arguments(tool_spec, &call.arguments) {
            self.reject_agent_tool_call_before_dispatch(
                cid,
                call,
                visible_tool_name,
                format!("invalid arguments for tool `{tool_name}`: {error}"),
            );
            return Ok(());
        }

        let call_id: ToolCallId = call.id.clone();

        // Track conversation attribution before publishing — the
        // publish path persists the `ToolRequest` into the session
        // log and folds it into the SessionTree via `apply_event`.
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tools.insert(
            call_id.clone(),
            PendingTool {
                name: visible_tool_name.clone(),
                internal_name: internal_tool_name.clone(),
                tool_type: call.tool_type,
            },
        );
        self.bump_tools_started_for(cid);
        self.record_wait_tool_request(&call_id);
        let published_request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: visible_tool_name.clone(),
            tool_type: call.tool_type,
            arguments: call.arguments.clone(),
            originator: tau_proto::PromptOriginator::User,
        };
        self.publish_for_conversation(cid, Event::ToolRequest(published_request));
        let request = ToolRequest {
            call_id: call_id.clone(),
            tool_name: internal_tool_name.clone(),
            tool_type: call.tool_type,
            arguments: call.arguments.clone(),
            originator: tau_proto::PromptOriginator::User,
        };

        match self.registry.route_tool_request(request) {
            Ok(route) => {
                let started = route.invoke;
                match route.target {
                    ToolRouteTarget::Internal => {
                        self.publish_for_conversation(cid, Event::ToolStarted(started));
                    }
                    ToolRouteTarget::Extension(provider_connection_id) => {
                        self.ensure_tool_started_subscription(&provider_connection_id);
                        self.pending_tool_providers
                            .insert(call_id.clone(), provider_connection_id);
                        self.publish_for_conversation(cid, Event::ToolStarted(started));
                    }
                }
            }
            Err(ToolRouteError::NoProvider { tool_name: _ }) => {
                let message = unavailable_tool_error_message(&visible_tool_name);
                self.publish_for_conversation(
                    cid,
                    Event::ToolRejected(ToolRejected {
                        call_id: call_id.clone(),
                        tool_name: visible_tool_name.clone(),
                        tool_type: call.tool_type,
                        message: message.clone(),
                        originator: tau_proto::PromptOriginator::User,
                    }),
                );
                let error = ToolError {
                    call_id: call_id.clone(),
                    tool_name: visible_tool_name.clone(),
                    tool_type: call.tool_type,
                    message,
                    details: None,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                };
                self.publish_terminal_tool_error(Some(cid), None, error);
                self.on_tool_call_complete(&call.id);
                self.clear_tool_call_tracking(call_id.as_str());
            }
            Err(error) => return Err(HarnessError::ToolRoute(error)),
        }

        Ok(())
    }
}

fn escape_agent_message_for_prompt(message: &str) -> String {
    let mut escaped = String::with_capacity(message.len());
    for ch in message.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

impl Harness {
    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    pub(crate) fn send_user_message(
        &mut self,
        session_id: &str,
        text: &str,
        _source_id: Option<&str>,
    ) -> Result<InteractionOutcome, HarnessError> {
        // Synchronous test entrypoint: dispatch directly without going
        // through `submit_user_prompt`'s queue. The embedded test harness
        // has no provider-published model (nothing to select from) and no UI
        // to drain a queued prompt, so the queued-until-model path would
        // deadlock. AGENTS.md session init is exercised separately in
        // unit tests via `submit_user_prompt` / manual turn-state setup.
        self.dispatch_user_prompt(session_id.into(), text.to_owned())?;

        let started_at = Instant::now();
        let mut progress_messages = Vec::new();
        loop {
            self.process_background_deadlines();
            let remaining = RESPONSE_TIMEOUT
                .checked_sub(started_at.elapsed())
                .unwrap_or(Duration::ZERO);
            let wait = self
                .tool_turn
                .next_background_deadline()
                .map(|deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(remaining)
                })
                .unwrap_or(remaining);
            let harness_evt = match self.rx.recv_timeout(wait) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout)
                    if started_at.elapsed() < RESPONSE_TIMEOUT
                        && self.tool_turn.next_background_deadline().is_some() =>
                {
                    self.process_background_deadlines();
                    continue;
                }
                Err(_) => return Err(HarnessError::ResponseTimeout),
            };
            self.log_event(&harness_evt);
            match harness_evt {
                HarnessEvent::FromConnection {
                    connection_id,
                    frame,
                } => {
                    if let Frame::Event(Event::ToolProgress(progress)) = frame.as_ref() {
                        progress_messages.push(format_tool_progress(progress));
                    }
                    let is_final = matches!(
                        frame.as_ref(),
                        Frame::Event(Event::ProviderResponseFinished(r))
                            if tool_calls_from_output_items(&r.output_items).is_empty()
                                && r.originator.is_user()
                    );
                    let final_text =
                        if let Frame::Event(Event::ProviderResponseFinished(r)) = frame.as_ref() {
                            assistant_text_from_output_items(&r.output_items)
                        } else {
                            None
                        };
                    self.handle_extension_event(&connection_id, *frame)?;
                    if is_final {
                        return Ok(InteractionOutcome {
                            lifecycle_messages: Vec::new(),
                            progress_messages,
                            response: final_text.unwrap_or_default(),
                        });
                    }
                }
                HarnessEvent::Disconnected { connection_id } => {
                    let was_provider = self.is_provider_extension(&connection_id);
                    self.handle_disconnect(&connection_id);
                    if was_provider {
                        return Err(provider_disconnected_error());
                    }
                }
                HarnessEvent::NewClient(_) => {}
                HarnessEvent::Command(command) => self.handle_harness_command(command)?,
            }
        }
    }

    pub(crate) fn dump_initial_prompt(
        out_path: &Path,
        user_message: &str,
    ) -> Result<(), HarnessError> {
        let tempdir = tempfile::TempDir::new()?;
        let state_dir = tempdir.path().join("state");
        let config = crate::settings::default_config();
        let mut harness = Self::from_config(
            &config,
            &state_dir,
            tau_config::settings::TauDirs::default(),
            "s1",
            tau_proto::SessionStartReason::Initial,
        )?;
        harness.selected_model = Some("test/model".parse().expect("model id"));

        let cid = harness.default_conversation_id.clone();
        harness.publish_event_for_conversation(
            &cid,
            None,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id: "s1".into(),
                text: user_message.to_owned(),
                target_agent_id: None,
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
        );

        let prompt_id = harness.send_prompt_to_agent_for(&cid);
        let prompt = harness.read_session_prompt_created(&prompt_id)?;
        let mut out = String::new();
        out.push_str("================ MODEL / EFFORT ================\n");
        out.push_str(&format!(
            "model:  {}\n",
            prompt
                .model
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "(none)".to_owned())
        ));
        out.push_str(&format!("params: {:?}\n\n", prompt.model_params));

        out.push_str("================ SYSTEM PROMPT ================\n");
        out.push_str(&prompt.system_prompt);
        if !prompt.system_prompt.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');

        out.push_str("================ CONTEXT ITEMS ================\n");
        out.push_str(
            &serde_json::to_string_pretty(&prompt.context_items)
                .map_err(|e| HarnessError::Participant(e.to_string()))?,
        );
        out.push_str("\n\n");

        out.push_str("================ TOOLS ================\n");
        out.push_str(
            &serde_json::to_string_pretty(&prompt.tools)
                .map_err(|e| HarnessError::Participant(e.to_string()))?,
        );
        out.push('\n');

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out_path, out)?;
        harness.shutdown()?;
        Ok(())
    }

    fn read_session_prompt_created(
        &self,
        prompt_id: &SessionPromptId,
    ) -> Result<SessionPromptCreated, HarnessError> {
        let mut cursor = 0;
        let mut snapshots = self.prompt_snapshots.clone();
        loop {
            let entry = self.event_log.get_next_from(cursor).ok_or_else(|| {
                HarnessError::Participant("prompt event missing from log".to_owned())
            })?;
            cursor = entry.seq + 1;
            if let Event::SessionPromptCreated(prompt) = entry.event {
                let mut materialized = prompt.clone();
                if let Some(tools_ref) = &prompt.tools_ref {
                    let base = snapshots
                        .get(&tools_ref.base_session_prompt_id)
                        .ok_or_else(|| {
                            HarnessError::Participant("prompt tools base missing".to_owned())
                        })?;
                    materialized.tools = base.tools.clone();
                    materialized.tools_ref = None;
                }
                snapshots.insert(materialized.session_prompt_id.clone(), materialized.clone());
                if &materialized.session_prompt_id == prompt_id {
                    return Ok(materialized);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    pub(crate) fn shutdown(&mut self) -> Result<(), HarnessError> {
        // Disconnect all extensions from the bus.  Dropping the
        // ChannelSink closes the writer channel, which triggers each
        // writer thread's shutdown sequence (send disconnect, close
        // stdin, wait/kill child). Walk `extension_order` so shutdown
        // honours spawn order.
        for id in &self.extension_order {
            let _ = self.bus.disconnect(id);
        }

        // Join in-process extension threads.
        let order = self.extension_order.clone();
        for id in &order {
            let Some(entry) = self.extensions.get_mut(id) else {
                continue;
            };
            let name = entry.name.clone();
            if let Some(handle) = entry.in_process_thread.take() {
                let result = handle
                    .join()
                    .map_err(|_| HarnessError::ThreadJoin(name.clone()))?;
                result.map_err(HarnessError::Participant)?;
            }
            self.emit_extension_exited(&name);
        }
        Ok(())
    }

    #[cfg(test)]
    fn extension_connection_id(&self, name: &str) -> Option<&str> {
        self.extensions
            .values()
            .find(|e| e.name == name)
            .map(|e| e.connection_id.as_str())
    }
}

fn provider_disconnected_error() -> HarnessError {
    HarnessError::Participant("provider disconnected".to_owned())
}

/// Pre-render the live-header descriptor for a tool call so the
/// CLI (and any future renderer) can paint the running block without
/// per-tool string knowledge. The descriptor carries the tool's
/// args label (e.g. `"foo" in src` for grep, `[task]` for delegate)
/// and is stamped with [`tau_proto::ToolDisplayStatus::InProgress`] /
/// [`tau_proto::PROGRESS_INDICATOR_TEXT`] so subscribers render the
/// running ellipsis uniformly.
///
/// Tools without a known label shape return `None`; the renderer
/// falls back to a name-only block.
/// True iff `anchor` is on the path from the tree root to
/// `descendant`. Used to check whether a previously-captured
/// stateful-chain anchor is still consistent with the conversation's
/// current head — branch switches (via `UiNavigateTree`) leave the
/// anchor stranded on a sibling branch, in which case the chain
/// should be invalidated and the next turn replays the full
/// transcript.
fn anchor_is_ancestor(
    tree: &tau_core::SessionTree,
    anchor: Option<tau_core::NodeId>,
    descendant: Option<tau_core::NodeId>,
) -> bool {
    // An empty-tree anchor matches an empty-tree descendant: both
    // sit at the root sentinel. Anything else with `anchor == None`
    // would be a malformed anchor (chain pinned to nothing).
    let Some(anchor) = anchor else {
        return descendant.is_none();
    };
    let mut current = descendant;
    while let Some(id) = current {
        if id == anchor {
            return true;
        }
        current = tree.node(id).and_then(|node| node.parent_id);
    }
    false
}

fn build_tool_args_display(
    tool_name: &str,
    arguments: &tau_proto::CborValue,
) -> Option<tau_proto::ToolDisplay> {
    use tau_proto::{ToolDisplayStatus, cbor_array_field, cbor_bool_field, cbor_text_field};

    let mut payload = None;
    let args = match tool_name {
        "shell" => {
            let command = cbor_text_field(arguments, "command").unwrap_or_default();
            payload = shell_command_payload(&command);
            shell_command_args(&command)
        }
        "read" => {
            let path = cbor_text_field(arguments, "path").unwrap_or_default();
            format!("{path} {}", format_requested_line_range(arguments))
        }
        "write" => cbor_text_field(arguments, "path").unwrap_or_default(),
        "edit" => {
            let path = cbor_text_field(arguments, "path").unwrap_or_default();
            let ranges = cbor_array_field(arguments, "edits")
                .map(format_requested_edit_ranges)
                .unwrap_or_default();
            if ranges.is_empty() {
                path
            } else {
                format!("{path} {ranges}")
            }
        }
        "find" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            format!("{pattern} in {path}")
        }
        "grep" => {
            let pattern = cbor_text_field(arguments, "pattern").unwrap_or_default();
            let path = cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned());
            let mut args = format!("{pattern:?} in {path}");
            if let Some(glob) = cbor_text_field(arguments, "glob") {
                args.push_str(&format!(" [{glob}]"));
            }
            args
        }
        "ls" => cbor_text_field(arguments, "path").unwrap_or_else(|| ".".to_owned()),
        "delegate" => match cbor_text_field(arguments, "task_name") {
            Some(name) if !name.is_empty() => match cbor_text_field(arguments, "role") {
                Some(role) if !role.is_empty() => format!("[{name}] +{role}"),
                _ => format!("[{name}]"),
            },
            _ => String::new(),
        },
        "skill" => {
            let query = cbor_query_label(arguments, "query");
            let scope = if cbor_bool_field(arguments, "search_content").unwrap_or(false) {
                " [content]"
            } else {
                ""
            };
            format!("{query}{scope}")
        }
        _ => return None,
    };
    Some(tau_proto::ToolDisplay {
        args,
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        payload,
        ..Default::default()
    })
}

fn format_requested_edit_ranges(edits: &[tau_proto::CborValue]) -> String {
    let mut ranges: Vec<String> = Vec::new();
    for edit in edits {
        let range = format_requested_line_range(edit);
        if ranges.iter().all(|existing| existing != &range) {
            ranges.push(range);
        }
    }
    ranges.join(",")
}

fn format_requested_line_range(arguments: &tau_proto::CborValue) -> String {
    let start_line = positive_usize_field(arguments, "start_line");
    let line_count = positive_usize_field(arguments, "line_count");
    match (start_line, line_count) {
        (None, None) => "..".to_owned(),
        (Some(start), None) => format!("{start}.."),
        (None, Some(count)) => format!("1..{}", 1usize.saturating_add(count)),
        (Some(start), Some(count)) => format!("{start}..{}", start.saturating_add(count)),
    }
}

fn positive_usize_field(arguments: &tau_proto::CborValue, key: &str) -> Option<usize> {
    let value = tau_proto::cbor_int_field(arguments, key)?;
    if value < 1 {
        return None;
    }
    usize::try_from(value).ok()
}

fn cbor_query_label(arguments: &tau_proto::CborValue, key: &str) -> String {
    let tau_proto::CborValue::Map(entries) = arguments else {
        return String::new();
    };
    let Some(value) = entries.iter().find_map(|(k, v)| match k {
        tau_proto::CborValue::Text(k) if k == key => Some(v),
        _ => None,
    }) else {
        return String::new();
    };
    match value {
        tau_proto::CborValue::Text(s) => skill_tool::normalized_skill_query_terms(s).join(" "),
        _ => String::new(),
    }
}

fn shell_command_args(command: &str) -> String {
    shorten_shell_command_line(command.lines().next().unwrap_or_default())
}

fn shorten_shell_command_line(line: &str) -> String {
    const EDGE_CHARS: usize = 20;
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= EDGE_CHARS * 2 {
        return line.to_owned();
    }

    let head: String = chars.iter().take(EDGE_CHARS).copied().collect();
    let tail: String = chars
        .iter()
        .skip(chars.len() - EDGE_CHARS)
        .copied()
        .collect();
    format!("{head}┄{tail}")
}

fn shell_command_payload(command: &str) -> Option<tau_proto::ToolDisplayPayload> {
    if command.lines().count() < 2 {
        return None;
    }
    Some(tau_proto::ToolDisplayPayload::Text {
        text: command.to_owned(),
    })
}

/// Build the [`ToolDisplay`] descriptor the renderer paints for a
/// running `delegate` tool block. Carries the sub-task name as the
/// args label and two progress counters (tools and context); the role and
/// execution mode stay on [`tau_proto::DelegateProgress`] so the UI can paint
/// them as dedicated chips. The tools counter is completed/total so
/// users can infer the currently running count as `total - completed`.
/// The trailing chip is set to
/// [`ToolDisplayStatus::InProgress`] so the renderer paints
/// [`tau_proto::PROGRESS_INDICATOR_TEXT`].
fn build_delegate_progress_display(
    task_name: &str,
    ctx_input_tokens: Option<u64>,
    ctx_percent: Option<u8>,
    ctx_window: Option<u64>,
    tools_in_flight: u32,
    tools_total: u32,
    input_stats: tau_proto::ToolDisplayStats,
) -> tau_proto::ToolDisplay {
    use tau_proto::{ProgressCounter, ProgressUnit, ToolDisplayStatus};

    let tools_completed = tools_total.saturating_sub(tools_in_flight);
    let mut counters: Vec<ProgressCounter> = vec![ProgressCounter {
        label: Some("tools".to_owned()),
        unit: ProgressUnit::Count,
        complete: Some(u64::from(tools_completed)),
        total: Some(u64::from(tools_total)),
    }];
    if ctx_input_tokens.is_some() || ctx_window.is_some() {
        counters.push(ProgressCounter {
            label: Some("ctx".to_owned()),
            unit: ProgressUnit::Tokens,
            complete: ctx_input_tokens,
            total: ctx_window,
        });
    } else if ctx_percent.is_some() {
        counters.push(ProgressCounter {
            label: Some("ctx".to_owned()),
            unit: ProgressUnit::Percent,
            complete: ctx_percent.map(u64::from),
            total: None,
        });
    }
    tau_proto::ToolDisplay {
        args: format!("[{task_name}]"),
        stats: input_stats,
        progress_counters: counters,
        status: ToolDisplayStatus::InProgress,
        status_text: tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        ..Default::default()
    }
}

/// Replace the `originator` on a tool-related event with the owning
/// conversation's originator. Non-tool events pass through unchanged.
fn stamp_tool_event_originator(event: Event, originator: tau_proto::PromptOriginator) -> Event {
    match event {
        Event::ToolRequest(mut e) => {
            e.originator = originator;
            Event::ToolRequest(e)
        }
        Event::ToolStarted(mut e) => {
            e.originator = originator;
            Event::ToolStarted(e)
        }
        Event::ToolRejected(mut e) => {
            e.originator = originator;
            Event::ToolRejected(e)
        }
        Event::ToolResult(mut e) => {
            e.originator = originator;
            Event::ToolResult(e)
        }
        Event::ToolError(mut e) => {
            e.originator = originator;
            Event::ToolError(e)
        }
        Event::ProviderToolResult(mut e) => {
            e.originator = originator;
            Event::ProviderToolResult(e)
        }
        Event::ProviderToolError(mut e) => {
            e.originator = originator;
            Event::ProviderToolError(e)
        }
        Event::ToolBackgroundResult(mut e) => {
            e.originator = originator;
            Event::ToolBackgroundResult(e)
        }
        Event::ToolBackgroundError(mut e) => {
            e.originator = originator;
            Event::ToolBackgroundError(e)
        }
        other => other,
    }
}

pub(crate) fn selector_matches_event(selectors: &[EventSelector], event: &Event) -> bool {
    let target_name = event.name();
    selectors.iter().any(|selector| match selector {
        EventSelector::Exact(expected) => *expected == target_name,
        EventSelector::Prefix(prefix) => target_name.matches_prefix(prefix),
    })
}

#[cfg(test)]
mod delegate_display_tests;

#[cfg(test)]
mod session_context_tests;
