//! Drains the event stream from the harness socket and paints it into
//! the terminal UI. Stateful: tracks per-prompt and per-tool-call UI
//! state so streaming updates land in the right block.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tau_proto::{
    CborValue, ContentPart, ContextItem, ContextRole, Event, MessageItem, ToolCallItem, UnixMicros,
};

use crate::action_commands::ActionCommandState;
use crate::build_banner;
use crate::tool_render::{
    CompactionStatus, ToolCallDisplay, ToolSummaryDisplay, build_delegate_completion_display,
    build_osc1337_set_user_var, build_tool_summary_display, extension_status_block, extract_diff,
    format_token_count, format_tool_call, render_compaction_block, render_delegate_display,
    render_diff_tool_block, render_harness_info, render_shell_block, render_tool_block,
    render_tool_display, render_turn_stats_block, session_status_block, streaming_block,
    synthesize_fallback_display, system_loaded_block, system_status_block, tool_duration_suffix,
    ui_dir_block,
};

pub(crate) struct EventRenderer {
    handle: tau_cli_term::TermHandle,
    completion_data: tau_cli_term::CompletionData,
    action_state: ActionCommandState,
    theme: tau_themes::Theme,
    /// Currently visible agent transcript. `None` means no agent has been
    /// selected yet; the renderer starts on the main interactive agent.
    current_agent_id: Option<String>,
    /// Output and renderer bookkeeping for agents that are not currently
    /// visible. The currently visible agent lives in the fields on this struct
    /// so existing rendering code can stay direct and efficient.
    agents_ui_state: HashMap<String, AgentUiState>,
    /// Agent ids known to the UI for `/agent` completion.
    known_agents: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Map side-query ids to the accepted agent id for routing prompt/provider
    /// events whose originator only carries `query_id`.
    query_agents: HashMap<String, String>,
    /// Map provider prompt ids to the agent transcript they belong to.
    prompt_agents: HashMap<String, String>,
    /// Map tool call ids to the agent transcript they belong to.
    tool_agents: HashMap<String, String>,
    /// Shared current visible agent mirror for prompt submission.
    current_agent_state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Per-`session_prompt_id` UI state. An entry is created on
    /// `SessionPromptCreated` (or `ProviderPromptSubmitted` for prompts
    /// without an explicit creation event) and torn down on
    /// `ProviderResponseFinished` or `SessionPromptTerminated`. Storing the
    /// response block id, thinking block id/text, and dispatch timestamp in one
    /// place means every
    /// per-prompt cleanup is a single `prompts.remove(spid)` instead of
    /// four separate `.remove()` calls easy to forget when extending.
    prompts: HashMap<String, PromptState>,
    /// Live provider-side compaction blocks keyed by session id.
    /// Compaction has first-class session lifecycle events rather than
    /// being inferred from the hidden compact prompt sent to the agent.
    compaction_blocks: HashMap<tau_proto::SessionId, tau_cli_term::BlockId>,
    /// Last locally-echoed user message that has not yet been classified
    /// as a normal or queued prompt. Used to replace only the matching
    /// echo when the harness reports that prompt as queued.
    last_user_block: Option<(tau_cli_term::BlockId, String)>,
    /// Queued user-message blocks (in above_sticky zone).
    /// When `SessionPromptCreated` fires for a dequeued prompt,
    /// the first entry is popped and moved back to history.
    queued_user_blocks: VecDeque<(tau_cli_term::BlockId, String)>,
    /// Per-`call_id` UI state. Tracks the live block (if any), the
    /// cached delegate args/progress for in-place re-renders, and
    /// whether the call belongs to a sub-agent side-conversation (in
    /// which case the UI suppresses its progress and result events).
    /// Entries are removed on terminal logical completion events.
    tool_calls: HashMap<String, ToolCallState>,
    /// Wakes the timer thread whenever visible tool activity starts or stops.
    tool_timer: Option<ToolTimerNotifier>,
    /// Live user-shell blocks (from `!`/`!!`) keyed by command_id.
    /// Updated in place as progress chunks arrive, finalized on
    /// `ShellCommandFinished`.
    shell_blocks: HashMap<String, ShellBlockState>,
    /// Live extension blocks keyed by instance_id. Shown in
    /// above_active while starting, moved to history when ready.
    extension_blocks: HashMap<tau_proto::ExtensionInstanceId, tau_cli_term::BlockId>,
    /// Extensions that are already up in this daemon. `/new` starts a
    /// fresh session, but these processes are intentionally kept.
    ready_extensions: HashSet<String>,
    /// Persistent status bar block showing the current model + effort.
    model_status_block: Option<tau_cli_term::BlockId>,
    /// Current session id, rendered as the last status-bar element.
    current_session_id: Option<tau_proto::SessionId>,
    /// Live history of completed write/edit blocks plus the data
    /// needed to re-render them. `/set show-diff` flips
    /// `diffs_expanded` and walks this list calling `set_block` so
    /// the entire transcript switches mode at once.
    diff_blocks: Vec<DiffBlockEntry>,
    /// Global expand-diffs toggle.
    diffs_expanded: bool,
    /// Global show-thinking toggle. When false, agent reasoning
    /// summaries are not rendered (live or in history). Controlled
    /// by `/set show-thinking`; persisted in `<state_dir>/cli.json`.
    show_thinking: bool,
    /// Persisted thinking blocks (one per finished assistant turn).
    /// When `show-thinking` flips, every entry is re-rendered as
    /// either the full text or removed, so the toggle takes effect
    /// retroactively across the visible transcript.
    thinking_history: Vec<ThinkingBlockEntry>,
    turn_stats_history: Vec<TurnStatsBlockEntry>,
    tool_history: Vec<ToolBlockEntry>,
    /// Durable message blocks and payloads, kept so `/set show-messages`
    /// can re-render the current transcript retroactively.
    message_history: Vec<MessageBlockEntry>,
    /// Where to persist `show_diff` / `show_thinking` /
    /// `show_turn_stats` / `show_tools` toggles.
    state_dirs: tau_config::settings::TauDirs,
    /// Model currently resolved for the selected role. `None` until the first
    /// `HarnessRoleSelected`, or while the selected role has no available
    /// provider-published model.
    current_model: Option<tau_proto::ModelId>,
    /// Currently selected agent role, as last announced by
    /// `HarnessRoleSelected`. `None` only before the first selection event.
    /// The status bar shows this instead of the derived model id.
    current_role: Option<String>,
    /// Current role details advertised for completion menus. Status
    /// chips compare against `baseline_params` instead, because these
    /// role details include persisted state overrides.
    role_defaults: HashMap<String, RoleCompletionDetails>,
    /// Role/provider baseline knobs for the current selection.
    /// Persisted state is intentionally excluded by the harness so the
    /// status bar can surface state adjustments from that baseline.
    baseline_params: Option<tau_proto::ModelParams>,
    /// Effective per-prompt model knobs derived from the selected role and
    /// role overrides. Mirrored into input-thread atomics for cycling helpers.
    model_params: tau_proto::ModelParams,
    /// Current model context usage percent. `None` when the context
    /// window is unknown for the selected model.
    current_context_percent: Option<u8>,
    /// Input tokens consumed by the most recent agent response. `None`
    /// until the first usage report for the current model.
    current_context_input_tokens: Option<u64>,
    /// Current model context window, in tokens, if known.
    current_context_window: Option<u64>,
    /// Main-agent tool calls completed for the current user task. Rendered
    /// in the status bar alongside [`Self::main_tools_total`].
    main_tools_completed: u64,
    /// Main-agent tool calls requested for the current user task. Sub-agent
    /// calls are excluded because they roll up under their `delegate` parent.
    main_tools_total: u64,
    /// Main-agent tool call ids whose foreground placeholder has returned, but
    /// whose real background result is still pending. These keep the status-bar
    /// tool chip visible and incomplete.
    main_backgrounded_tools: HashSet<String>,
    /// Whether the currently active prompt/agent lifecycle belongs to the
    /// user-facing main agent. Side conversations temporarily make this
    /// false while preserving the main task's counters.
    main_agent_turn_active: bool,
    /// Whether the main-agent tool usage chip should be painted. This is
    /// separate from the counters so side-conversation lifecycles can hide
    /// the chip until a main lifecycle event makes the main turn active
    /// again.
    main_tools_visible: bool,
    /// Whether to render per-turn token usage stats below completed
    /// agent responses.
    show_turn_stats: bool,
    /// Whether to show a temporary full-redraw counter in the status bar.
    redraw_counter: bool,
    last_full_render_count: u64,
    last_full_render_at: Option<Instant>,
    /// Tool block visibility mode.
    show_tools: tau_config::settings::ShowTools,
    /// Agent/user message visibility mode.
    show_messages: tau_config::settings::ShowMessages,
    /// Tool summary blocks keyed by their block id. Hidden when
    /// `show_tools` is `Full` or `Compact`, rendered in summarize modes.
    tool_summaries: HashMap<tau_cli_term::BlockId, ToolSummaryDisplay>,
    /// In `summarize-prompt` mode, the single summary block for the
    /// active user prompt. Reused across the follow-up agent turns the
    /// harness creates while feeding tool results back to the model.
    prompt_tool_summary: Option<tau_cli_term::BlockId>,
    /// Whether [`Self::prompt_tool_summary`] currently lives in the bottom
    /// active-tools area. In `summarize-prompt` mode the summary stays sticky
    /// across tool follow-up turns, then moves to history when the assistant
    /// finishes without requesting more tools.
    prompt_tool_summary_active: bool,
    /// Snapshot of persisted CLI settings, kept in sync with the four
    /// `show_*` fields above by [`Self::save_cli_state`]. The input
    /// loop captures this handle in the `/set` name-completion
    /// closure so the menu can show each setting's current value
    /// without snooping on renderer-thread fields directly.
    cli_state_mirror: std::sync::Arc<std::sync::Mutex<tau_config::settings::CliState>>,
    /// Cumulative end-to-end time spent waiting for agent responses.
    cumulative_agent_latency: Duration,
    /// Shared effort mirror kept in sync with harness state.
    effort_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Shared fast-service-tier mirror for the input thread's `fast-toggle`
    /// binding.
    fast_service_tier_state: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Shared active-role mirror for input-thread role cycling.
    current_role_state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Shared ordered role names for input-thread role cycling.
    roles_available: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Shared ordered role groups for input-thread role cycling.
    role_groups_available: std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessRoleGroup>>>,
    /// Last selected role per role group for in-memory group cycling.
    role_group_memory: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Shared verbosity mirror kept symmetric with `effort_state`.
    verbosity_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Shared thinking-summary mirror. Kept symmetric with the
    /// other knobs for future cycle helpers.
    thinking_summary_state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Context appended to files opened by the external prompt editor.
    /// Locked with `if let Ok(...)` rather than [`crate::locked`] because
    /// this is best-effort UI metadata: if another holder panicked we'd
    /// rather drop one editor-context update than crash the renderer
    /// thread.
    editor_context: std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>>,
    /// Symbol shown before the active prompt input.
    prompt_symbol: String,
    /// Symbol shown before submitted prompts in the transcript.
    submitted_prompt_symbol: String,
    /// Shared flag telling the input loop whether Tau knows about
    /// in-flight agent/session work. Updated before side-conversation
    /// filtering so sub-agent activity protects Ctrl-D too.
    agent_in_progress: Arc<AtomicBool>,
    /// Detailed lifecycle bookkeeping backing [`Self::agent_in_progress`].
    agent_activity: AgentActivity,
}

const MAIN_AGENT_ID: &str = "main";

#[derive(Default)]
struct AgentUiState {
    output: tau_cli_term::OutputSnapshot,
    prompts: HashMap<String, PromptState>,
    compaction_blocks: HashMap<tau_proto::SessionId, tau_cli_term::BlockId>,
    last_user_block: Option<(tau_cli_term::BlockId, String)>,
    queued_user_blocks: VecDeque<(tau_cli_term::BlockId, String)>,
    tool_calls: HashMap<String, ToolCallState>,
    shell_blocks: HashMap<String, ShellBlockState>,
    model_status_block: Option<tau_cli_term::BlockId>,
    diff_blocks: Vec<DiffBlockEntry>,
    thinking_history: Vec<ThinkingBlockEntry>,
    turn_stats_history: Vec<TurnStatsBlockEntry>,
    tool_history: Vec<ToolBlockEntry>,
    message_history: Vec<MessageBlockEntry>,
    current_context_percent: Option<u8>,
    current_context_input_tokens: Option<u64>,
    current_context_window: Option<u64>,
    main_tools_completed: u64,
    main_tools_total: u64,
    main_backgrounded_tools: HashSet<String>,
    main_agent_turn_active: bool,
    main_tools_visible: bool,
    tool_summaries: HashMap<tau_cli_term::BlockId, ToolSummaryDisplay>,
    prompt_tool_summary: Option<tau_cli_term::BlockId>,
    prompt_tool_summary_active: bool,
    cumulative_agent_latency: Duration,
    agent_activity: AgentActivity,
}

/// One completed file-mutation tool block. Held so `/set show-diff` can
/// re-render every diff in the chat history when the global
/// expand toggle flips.
struct DiffBlockEntry {
    block_id: tau_cli_term::BlockId,
    display: ToolCallDisplay,
    diff: tau_proto::DiffSummary,
}

#[derive(Clone)]
pub(crate) struct ToolTimerNotifier {
    inner: Arc<(std::sync::Mutex<ToolTimerState>, std::sync::Condvar)>,
}

pub(crate) struct ToolTimerState {
    pub(crate) active_tool_ids: HashSet<String>,
    pub(crate) done: bool,
}

impl ToolTimerNotifier {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new((
                std::sync::Mutex::new(ToolTimerState {
                    active_tool_ids: HashSet::new(),
                    done: false,
                }),
                std::sync::Condvar::new(),
            )),
        }
    }

    pub(crate) fn inner(&self) -> Arc<(std::sync::Mutex<ToolTimerState>, std::sync::Condvar)> {
        self.inner.clone()
    }

    fn tool_started(&self, call_id: &str) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.active_tool_ids.insert(call_id.to_owned());
            cv.notify_all();
        }
    }

    fn tool_finished(&self, call_id: &str) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.active_tool_ids.remove(call_id);
            cv.notify_all();
        }
    }

    fn clear_active(&self) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.active_tool_ids.clear();
            cv.notify_all();
        }
    }

    pub(crate) fn stop(&self) {
        let (mutex, cv) = &*self.inner;
        if let Ok(mut state) = mutex.lock() {
            state.done = true;
            cv.notify_all();
        }
    }
}

struct ToolBlockEntry {
    block_id: tau_cli_term::BlockId,
    display: ToolCallDisplay,
}

struct MessageBlockEntry {
    block_id: tau_cli_term::BlockId,
    message: tau_proto::AgentMessage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageRenderMode {
    Hidden,
    Summary,
    Full,
}

/// One finished thinking block. Held so `/set show-thinking` can swap
/// its content between the original reasoning text (visible) and
/// empty content (hidden) without losing the block's position in
/// the transcript.
struct ThinkingBlockEntry {
    block_id: tau_cli_term::BlockId,
    text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RoleCompletionDetails {
    model: Option<String>,
    effort: Option<String>,
    verbosity: Option<String>,
    thinking_summary: Option<String>,
    service_tier: Option<String>,
    tools: Option<String>,
    enable_tools: Option<String>,
    disable_tools: Option<String>,
    role_description: Option<String>,
}

impl RoleCompletionDetails {
    fn from_role_info(role: &tau_proto::HarnessRoleInfo) -> Self {
        let mut details = Self::from_description(&role.description);
        details.role_description = role.role_description.clone();
        details
    }

    fn from_description(description: &str) -> Self {
        let mut details = Self {
            model: None,
            effort: None,
            verbosity: None,
            thinking_summary: None,
            service_tier: None,
            tools: None,
            enable_tools: None,
            disable_tools: None,
            role_description: None,
        };

        if description == "no model" {
            return details;
        }

        for part in description.split(',').map(str::trim) {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "model" => details.model = Some(value.to_owned()),
                "effort" => details.effort = Some(value.to_owned()),
                "verbosity" => details.verbosity = Some(value.to_owned()),
                "thinking-summary" => details.thinking_summary = Some(value.to_owned()),
                "service-tier" => details.service_tier = Some(value.to_owned()),
                "tools" => details.tools = Some(value.to_owned()),
                "enable-tools" => details.enable_tools = Some(value.to_owned()),
                "disable-tools" => details.disable_tools = Some(value.to_owned()),
                _ => {}
            }
        }

        details
    }

    fn short_description(&self) -> String {
        let mut parts = Vec::new();
        if let Some(model) = self.model.as_deref() {
            parts.push(model.to_owned());
        }
        if let Some(effort) = self.effort.as_deref() {
            parts.push(format!("e={effort}"));
        }
        if let Some(verbosity) = self.verbosity.as_deref() {
            parts.push(format!("v={verbosity}"));
        }
        if let Some(thinking_summary) = self.thinking_summary.as_deref() {
            parts.push(format!("ts={thinking_summary}"));
        }
        if let Some(service_tier) = self.service_tier.as_deref() {
            parts.push(format!("st={service_tier}"));
        }
        if let Some(tools) = self.tools.as_deref() {
            parts.push(format!("tools={tools}"));
        }
        if let Some(enable_tools) = self.enable_tools.as_deref() {
            parts.push(format!("et={enable_tools}"));
        }
        if let Some(disable_tools) = self.disable_tools.as_deref() {
            parts.push(format!("dt={disable_tools}"));
        }
        let mut summary = if parts.is_empty() {
            "no model".to_owned()
        } else {
            parts.join(" ")
        };
        if let Some(description) = self.role_description.as_deref() {
            let description = description.trim();
            if !description.is_empty() {
                summary.push_str(" — ");
                summary.push_str(description);
            }
        }
        summary
    }

    fn current_description(&self, field: &str) -> String {
        match field {
            "model" => self.model.as_deref().unwrap_or("unset").to_owned(),
            "effort" => self.effort.as_deref().unwrap_or("unset").to_owned(),
            "verbosity" => self.verbosity.as_deref().unwrap_or("unset").to_owned(),
            "thinking-summary" => self
                .thinking_summary
                .as_deref()
                .unwrap_or("unset")
                .to_owned(),
            "service-tier" => self.service_tier.as_deref().unwrap_or("unset").to_owned(),
            "tools" => self.tools.as_deref().unwrap_or("unset").to_owned(),
            "enable-tools" => self.enable_tools.as_deref().unwrap_or("unset").to_owned(),
            "disable-tools" => self.disable_tools.as_deref().unwrap_or("unset").to_owned(),
            _ => "unset".to_owned(),
        }
    }
}

fn role_value_completion(setting: &str, value: &str) -> tau_cli_term::CompletionItem {
    let description = match (setting, value) {
        (_, "reset") => "clear this role setting",
        ("effort", "off") => "disable reasoning effort",
        ("effort", "minimal") => "minimum reasoning effort",
        ("effort", "low") => "light reasoning effort",
        ("effort", "medium") => "balanced reasoning effort",
        ("effort", "high") => "strong reasoning effort",
        ("effort", "xhigh") => "maximum reasoning effort",
        ("verbosity", "low") => "terse responses",
        ("verbosity", "medium") => "normal responses",
        ("verbosity", "high") => "detailed responses",
        ("thinking-summary", "off") => "hide thinking summaries",
        ("thinking-summary", "auto") => "provider default summaries",
        ("thinking-summary", "concise") => "short thinking summaries",
        ("thinking-summary", "detailed") => "detailed thinking summaries",
        ("service-tier", "fast") => "use fast service tier",
        ("service-tier", "flex") => "use flex service tier",
        _ => "",
    };
    tau_cli_term::CompletionItem::new(value, description)
}

fn role_completion_matches(value: &str, needle: &str) -> bool {
    needle.is_empty() || value.starts_with(needle) || value.contains(needle)
}

fn empty_role_completion_details() -> RoleCompletionDetails {
    RoleCompletionDetails {
        model: None,
        effort: None,
        verbosity: None,
        thinking_summary: None,
        service_tier: None,
        tools: None,
        enable_tools: None,
        disable_tools: None,
        role_description: None,
    }
}

fn role_setting_completions(
    details: &RoleCompletionDetails,
    needle: &str,
) -> Vec<tau_cli_term::CompletionItem> {
    [
        ("delete", "delete this runtime role/override".to_owned()),
        ("model", details.current_description("model")),
        ("effort", details.current_description("effort")),
        ("verbosity", details.current_description("verbosity")),
        (
            "thinking-summary",
            details.current_description("thinking-summary"),
        ),
        ("service-tier", details.current_description("service-tier")),
        ("tools", details.current_description("tools")),
        ("enable-tools", details.current_description("enable-tools")),
        (
            "disable-tools",
            details.current_description("disable-tools"),
        ),
    ]
    .into_iter()
    .filter(|(value, _)| role_completion_matches(value, needle))
    .map(|(value, desc)| tau_cli_term::CompletionItem::new(value, desc))
    .collect()
}

fn role_setting_value_completions(
    setting: &str,
    needle: &str,
) -> Vec<tau_cli_term::CompletionItem> {
    let values: &[&str] = match setting {
        "model" | "tools" | "enable-tools" | "disable-tools" => &["reset"],
        "effort" => &["reset", "off", "minimal", "low", "medium", "high", "xhigh"],
        "verbosity" => &["reset", "low", "medium", "high"],
        "thinking-summary" => &["reset", "off", "auto", "concise", "detailed"],
        "service-tier" => &["reset", "fast", "flex"],
        _ => &[],
    };
    values
        .iter()
        .copied()
        .filter(|value| role_completion_matches(value, needle))
        .map(|value| role_value_completion(setting, value))
        .collect()
}

fn role_command_completions(
    role_items: &[(tau_cli_term::CompletionItem, RoleCompletionDetails)],
    args: &[&str],
) -> Vec<tau_cli_term::CompletionItem> {
    match args.len() {
        1 => role_items
            .iter()
            .filter(|(item, _)| role_completion_matches(&item.value, args[0]))
            .map(|(item, _)| item.clone())
            .collect(),
        2 => {
            let details = role_items
                .iter()
                .find(|(item, _)| item.value == args[0])
                .map(|(_, details)| details.clone())
                .unwrap_or_else(empty_role_completion_details);
            role_setting_completions(&details, args[1])
        }
        3 => role_setting_value_completions(args[1], args[2]),
        _ => Vec::new(),
    }
}

struct TurnStatsBlockEntry {
    block_id: tau_cli_term::BlockId,
    usage: tau_proto::ProviderTokenUsage,
    turn_latency: Option<Duration>,
    total_latency: Option<Duration>,
}

/// Per-prompt UI state held by [`EventRenderer`]. Lives from the first
/// event observed for the prompt (`SessionPromptCreated` or
/// `ProviderPromptSubmitted`) through `ProviderResponseFinished` or
/// `SessionPromptTerminated`.
#[derive(Default)]
struct PromptState {
    /// Live agent-response block. `None` until `SessionPromptCreated`
    /// allocates it (some prompts arrive without a creation event).
    response_block_id: Option<tau_cli_term::BlockId>,
    /// Live thinking block. Lazy-created the first time the agent emits
    /// non-empty `thinking`, so backends that don't return reasoning
    /// summaries produce no extra block.
    thinking_block_id: Option<tau_cli_term::BlockId>,
    /// Latest captured thinking text. Held so `ProviderResponseFinished`
    /// can render it into history even when the finish event doesn't
    /// carry its own `thinking` payload.
    thinking_text: Option<String>,
    /// Dispatch timestamp, used to compute end-to-end latency on
    /// `ProviderResponseFinished`.
    started_at: Option<Instant>,
}

/// Per-tool-call UI state held by [`EventRenderer`]. Created when the
/// harness publishes `ToolStarted` (or when a sub-agent's finish marks the call
/// as suppressed) and torn down on `ToolResult`/`ToolError`.
#[derive(Default)]
struct ToolCallState {
    /// Live tool-call block in the active-tools area. `None` for sub-agent
    /// tool calls whose UI is suppressed (their progress is rolled up into the
    /// parent `delegate` block via `DelegateProgress` instead).
    block_id: Option<tau_cli_term::BlockId>,
    /// Empty history placeholder allocated at the tool call's logical
    /// transcript position. Final results fill this block so live progress
    /// can update the bottom active-tools area without mutating old
    /// transcript rows.
    history_block_id: Option<tau_cli_term::BlockId>,
    /// Latest live display for the block, used when `/set show-tools`
    /// flips while the call is still running.
    live_display: Option<ToolCallDisplay>,
    /// Monotonic start time for live duration updates.
    started_at: Option<Instant>,
    /// Harness log timestamp for final duration chips.
    recorded_started_at: Option<UnixMicros>,
    /// Summary block for the assistant tool batch this call belongs
    /// to. `None` for stray events without a preceding tool-call
    /// announcement.
    summary_block_id: Option<tau_cli_term::BlockId>,
    /// Most recent `DelegateProgress` snapshot. On `ToolResult` we
    /// render the completion line with the final `#…` / `%…`
    /// chips so the user sees the delegation cost alongside the
    /// response stats.
    delegate_last_progress: Option<tau_proto::DelegateProgress>,
    /// `true` for the user-facing parent `delegate` tool call that
    /// spawned a side conversation. While it is live, side-conversation
    /// prompt lifecycle events must not hide the main tool usage chip.
    is_main_delegate: bool,
    /// `true` for tool calls in side conversations. Their lifecycle
    /// events (`ToolResult`, `ToolError`, `ToolProgress`) share the bus
    /// with the main agent's, but the UI filters them out.
    is_sub_agent: bool,
}

/// In-flight state for a user `!`/`!!` shell block.
struct ShellBlockState {
    block_id: tau_cli_term::BlockId,
    command: String,
    include_in_context: bool,
    /// Output accumulated from `ShellCommandProgress` chunks. Rendered
    /// under the header each redraw.
    output: String,
}

/// Session/agent lifecycle state used to decide whether Ctrl-D is safe.
#[derive(Default)]
struct AgentActivity {
    /// User submissions seen before the harness assigns a prompt id.
    optimistic_submissions: usize,
    /// Prompt ids currently being processed by any provider conversation.
    active_prompts: HashSet<String>,
    /// Tool call ids emitted by any agent and not finished yet.
    active_tools: HashSet<String>,
    /// Tool call ids whose foreground provider protocol has completed with a
    /// synthetic placeholder, but whose real tool process is still running.
    backgrounded_tools: HashSet<String>,
}

impl AgentActivity {
    fn is_in_progress(&self) -> bool {
        self.optimistic_submissions != 0
            || !self.active_prompts.is_empty()
            || !self.active_tools.is_empty()
    }

    fn mark_optimistic_submission(&mut self) {
        self.optimistic_submissions = self.optimistic_submissions.saturating_add(1);
    }

    fn start_prompt(&mut self, session_prompt_id: &tau_proto::SessionPromptId) {
        self.optimistic_submissions = self.optimistic_submissions.saturating_sub(1);
        self.active_prompts.insert(session_prompt_id.to_string());
    }

    fn finish_prompt(
        &mut self,
        session_prompt_id: &tau_proto::SessionPromptId,
        output_items: &[ContextItem],
    ) {
        if self.active_prompts.remove(session_prompt_id.as_str()) {
            for call in tool_calls_from_output_items(output_items) {
                self.active_tools.insert(call.call_id.to_string());
            }
        } else {
            self.optimistic_submissions = self.optimistic_submissions.saturating_sub(1);
        }
    }

    fn start_tool(&mut self, call_id: &tau_proto::ToolCallId) {
        self.active_tools.insert(call_id.to_string());
    }

    fn background_tool(&mut self, call_id: &tau_proto::ToolCallId) {
        self.backgrounded_tools.insert(call_id.to_string());
        self.active_tools.insert(call_id.to_string());
    }

    fn finish_tool(&mut self, call_id: &tau_proto::ToolCallId) {
        if !self.backgrounded_tools.contains(call_id.as_str()) {
            self.active_tools.remove(call_id.as_str());
        }
    }

    fn finish_background_tool(&mut self, call_id: &tau_proto::ToolCallId) {
        self.backgrounded_tools.remove(call_id.as_str());
        self.active_tools.remove(call_id.as_str());
    }

    fn clear_optimistic_submissions(&mut self) {
        self.optimistic_submissions = 0;
    }

    fn clear(&mut self) {
        self.optimistic_submissions = 0;
        self.active_prompts.clear();
        self.active_tools.clear();
        self.backgrounded_tools.clear();
    }
}

/// Returns the originator of any prompt-lifecycle event, or
/// [`tau_proto::PromptOriginator::User`] for events that don't carry
/// one (so unrelated events render as before).
fn originator_of(event: &Event) -> tau_proto::PromptOriginator {
    match event {
        Event::UiPromptSubmitted(p) => p.originator.clone(),
        Event::SessionPromptCreated(p) => p.originator.clone(),
        Event::SessionPromptTerminated(t) => t.originator.clone(),
        Event::ProviderPromptSubmitted(s) => s.originator.clone(),
        Event::ProviderResponseUpdated(u) => u.originator.clone(),
        Event::ProviderResponseFinished(f) => f.originator.clone(),
        Event::ProviderToolResult(result) => result.originator.clone(),
        Event::ProviderToolError(error) => error.originator.clone(),
        Event::SessionCompactionStarted(started) => started.originator.clone(),
        Event::SessionCompactionFinished(finished) => finished.originator.clone(),
        Event::SessionCompacted(compacted) => compacted.originator.clone(),
        _ => tau_proto::PromptOriginator::User,
    }
}

fn push_status_chip(
    themed: &mut tau_themes::ThemedText,
    style: tau_themes::StyleIdx,
    needs_space: &mut bool,
    text: impl Into<String>,
) {
    if *needs_space {
        themed.push_default(" ");
    }
    themed.push(style, text.into());
    *needs_space = true;
}

fn assistant_text_from_output_items(output_items: &[ContextItem]) -> Option<String> {
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

fn assistant_text_from_message_item(message: &MessageItem) -> Option<String> {
    if message.role != ContextRole::Assistant {
        return None;
    }
    let text = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn tool_calls_from_output_items(output_items: &[ContextItem]) -> Vec<ToolCallItem> {
    output_items
        .iter()
        .filter_map(|item| match item {
            ContextItem::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn cbor_text_field(arguments: &CborValue, key: &str) -> Option<String> {
    let CborValue::Map(entries) = arguments else {
        return None;
    };
    entries
        .iter()
        .find_map(|(entry_key, value)| match (entry_key, value) {
            (CborValue::Text(entry_key), CborValue::Text(value)) if entry_key == key => {
                Some(value.clone())
            }
            _ => None,
        })
}

fn tool_display_from_call(call: &ToolCallItem) -> tau_proto::ToolDisplay {
    if call.name.as_str() == "shell" {
        let command = cbor_text_field(&call.arguments, "command").unwrap_or_default();
        return shell_tool_display_from_command(command);
    }

    let args = match call.name.as_str() {
        "read" | "write" | "edit" | "ls" => cbor_text_field(&call.arguments, "path"),
        "grep" | "glob" => cbor_text_field(&call.arguments, "pattern"),
        "delegate" => cbor_text_field(&call.arguments, "task_name"),
        _ => cbor_text_field(&call.arguments, "path")
            .or_else(|| cbor_text_field(&call.arguments, "pattern"))
            .or_else(|| cbor_text_field(&call.arguments, "query")),
    }
    .unwrap_or_default();
    in_progress_tool_display(args, None)
}

fn shell_tool_display_from_command(command: String) -> tau_proto::ToolDisplay {
    // Mirror ext-shell's final display shape so `show-tools=full` does not
    // change layout when a multiline command finishes.
    let args = command.lines().next().unwrap_or_default().to_owned();
    let payload = (2 <= command.lines().count())
        .then_some(tau_proto::ToolDisplayPayload::Text { text: command });
    in_progress_tool_display(args, payload)
}

fn in_progress_tool_display(
    args: String,
    payload: Option<tau_proto::ToolDisplayPayload>,
) -> tau_proto::ToolDisplay {
    tau_proto::ToolDisplay {
        args,
        payload,
        status: tau_proto::ToolDisplayStatus::InProgress,
        status_text: "…".to_owned(),
        ..Default::default()
    }
}

impl EventRenderer {
    #[cfg(test)]
    pub(crate) fn new(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
    ) -> Self {
        // Tests pass a state_dir of None so toggles never touch the
        // user's real `~/.local/state/tau/cli.json`.
        Self::new_with_state(
            handle,
            completion_data,
            theme,
            tau_config::settings::CliState::default(),
            tau_config::settings::TauDirs {
                config_dir: None,
                state_dir: None,
            },
            ">".to_string(),
            ">".to_string(),
        )
    }

    pub(crate) fn new_with_state(
        handle: tau_cli_term::TermHandle,
        completion_data: tau_cli_term::CompletionData,
        theme: tau_themes::Theme,
        state: tau_config::settings::CliState,
        state_dirs: tau_config::settings::TauDirs,
        prompt_symbol: String,
        submitted_prompt_symbol: String,
    ) -> Self {
        let cli_state_mirror = std::sync::Arc::new(std::sync::Mutex::new(state.clone()));
        Self {
            handle,
            completion_data,
            action_state: ActionCommandState::new(std::iter::empty::<&str>()),
            theme,
            current_agent_id: None,
            agents_ui_state: HashMap::new(),
            known_agents: std::sync::Arc::new(std::sync::Mutex::new(vec![
                MAIN_AGENT_ID.to_owned(),
            ])),
            query_agents: HashMap::new(),
            prompt_agents: HashMap::new(),
            tool_agents: HashMap::new(),
            current_agent_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
            prompts: HashMap::new(),
            compaction_blocks: HashMap::new(),
            last_user_block: None,
            queued_user_blocks: VecDeque::new(),
            tool_calls: HashMap::new(),
            tool_timer: None,
            shell_blocks: HashMap::new(),
            extension_blocks: HashMap::new(),
            ready_extensions: HashSet::new(),
            model_status_block: None,
            current_session_id: None,
            diff_blocks: Vec::new(),
            diffs_expanded: state.show_diff,
            show_thinking: state.show_thinking,
            show_turn_stats: state.show_turn_stats,
            show_tools: state.show_tools,
            show_messages: state.show_messages,
            tool_summaries: HashMap::new(),
            prompt_tool_summary: None,
            prompt_tool_summary_active: false,
            cli_state_mirror,
            thinking_history: Vec::new(),
            turn_stats_history: Vec::new(),
            tool_history: Vec::new(),
            message_history: Vec::new(),
            state_dirs,
            current_model: None,
            current_role: None,
            model_params: tau_proto::ModelParams::default(),
            role_defaults: HashMap::new(),
            baseline_params: None,
            current_context_percent: None,
            current_context_input_tokens: None,
            current_context_window: None,
            main_tools_completed: 0,
            main_tools_total: 0,
            main_backgrounded_tools: HashSet::new(),
            main_agent_turn_active: false,
            main_tools_visible: false,
            redraw_counter: state.redraw_counter,
            last_full_render_count: 0,
            last_full_render_at: None,
            cumulative_agent_latency: Duration::ZERO,
            effort_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                tau_proto::Effort::Off.as_u8(),
            )),
            fast_service_tier_state: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            current_role_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
            roles_available: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            role_groups_available: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            role_group_memory: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            verbosity_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                tau_proto::Verbosity::default().as_u8(),
            )),
            thinking_summary_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
                tau_proto::ThinkingSummary::default().as_u8(),
            )),
            editor_context: std::sync::Arc::new(std::sync::Mutex::new(
                tau_cli_term::EditorContext::default(),
            )),
            prompt_symbol,
            submitted_prompt_symbol,
            agent_in_progress: Arc::new(AtomicBool::new(false)),
            agent_activity: AgentActivity::default(),
        }
    }

    pub(crate) fn set_tool_timer(&mut self, timer: ToolTimerNotifier) {
        self.tool_timer = Some(timer);
    }

    pub(crate) fn known_agents(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        self.known_agents.clone()
    }

    pub(crate) fn current_agent_state(&self) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
        self.current_agent_state.clone()
    }

    pub(crate) fn switch_agent(&mut self, agent_id: String) {
        self.remember_agent(agent_id.clone());
        if self.current_agent_id.is_none() {
            self.current_agent_id = Some(agent_id.clone());
            if let Ok(mut current) = self.current_agent_state.lock() {
                *current = Some(agent_id);
            }
            self.render_model_status();
            return;
        }
        if self.current_agent_id.as_deref() == Some(agent_id.as_str()) {
            return;
        }
        if let Some(current) = self.current_agent_id.clone() {
            let state = self.take_visible_agent_state();
            self.agents_ui_state.insert(current, state);
        }
        let state = self.agents_ui_state.remove(&agent_id).unwrap_or_default();
        self.restore_visible_agent_state(state);
        self.current_agent_id = Some(agent_id.clone());
        if let Ok(mut current) = self.current_agent_state.lock() {
            *current = Some(agent_id);
        }
        self.render_model_status();
    }

    pub(crate) fn set_action_state(&mut self, action_state: ActionCommandState) {
        self.action_state = action_state;
        self.refresh_action_completions();
    }

    fn take_visible_agent_state(&mut self) -> AgentUiState {
        AgentUiState {
            output: self.handle.output_snapshot(),
            prompts: std::mem::take(&mut self.prompts),
            compaction_blocks: std::mem::take(&mut self.compaction_blocks),
            last_user_block: self.last_user_block.take(),
            queued_user_blocks: std::mem::take(&mut self.queued_user_blocks),
            tool_calls: std::mem::take(&mut self.tool_calls),
            shell_blocks: std::mem::take(&mut self.shell_blocks),
            model_status_block: self.model_status_block.take(),
            diff_blocks: std::mem::take(&mut self.diff_blocks),
            thinking_history: std::mem::take(&mut self.thinking_history),
            turn_stats_history: std::mem::take(&mut self.turn_stats_history),
            tool_history: std::mem::take(&mut self.tool_history),
            message_history: std::mem::take(&mut self.message_history),
            current_context_percent: self.current_context_percent.take(),
            current_context_input_tokens: self.current_context_input_tokens.take(),
            current_context_window: self.current_context_window.take(),
            main_tools_completed: std::mem::take(&mut self.main_tools_completed),
            main_tools_total: std::mem::take(&mut self.main_tools_total),
            main_backgrounded_tools: std::mem::take(&mut self.main_backgrounded_tools),
            main_agent_turn_active: std::mem::take(&mut self.main_agent_turn_active),
            main_tools_visible: std::mem::take(&mut self.main_tools_visible),
            tool_summaries: std::mem::take(&mut self.tool_summaries),
            prompt_tool_summary: self.prompt_tool_summary.take(),
            prompt_tool_summary_active: std::mem::take(&mut self.prompt_tool_summary_active),
            cumulative_agent_latency: std::mem::take(&mut self.cumulative_agent_latency),
            agent_activity: std::mem::take(&mut self.agent_activity),
        }
    }

    fn restore_visible_agent_state(&mut self, state: AgentUiState) {
        self.restore_visible_agent_state_inner(state, true);
    }

    fn restore_hidden_agent_state(&mut self, state: AgentUiState) {
        self.restore_visible_agent_state_inner(state, false);
    }

    fn restore_visible_agent_state_inner(&mut self, state: AgentUiState, redraw: bool) {
        if redraw {
            self.handle.replace_output_snapshot(state.output);
        } else {
            self.handle.replace_output_snapshot_quiet(state.output);
        }
        self.prompts = state.prompts;
        self.compaction_blocks = state.compaction_blocks;
        self.last_user_block = state.last_user_block;
        self.queued_user_blocks = state.queued_user_blocks;
        self.tool_calls = state.tool_calls;
        self.shell_blocks = state.shell_blocks;
        self.model_status_block = state.model_status_block;
        self.diff_blocks = state.diff_blocks;
        self.thinking_history = state.thinking_history;
        self.turn_stats_history = state.turn_stats_history;
        self.tool_history = state.tool_history;
        self.message_history = state.message_history;
        self.current_context_percent = state.current_context_percent;
        self.current_context_input_tokens = state.current_context_input_tokens;
        self.current_context_window = state.current_context_window;
        self.main_tools_completed = state.main_tools_completed;
        self.main_tools_total = state.main_tools_total;
        self.main_backgrounded_tools = state.main_backgrounded_tools;
        self.main_agent_turn_active = state.main_agent_turn_active;
        self.main_tools_visible = state.main_tools_visible;
        self.tool_summaries = state.tool_summaries;
        self.prompt_tool_summary = state.prompt_tool_summary;
        self.prompt_tool_summary_active = state.prompt_tool_summary_active;
        self.cumulative_agent_latency = state.cumulative_agent_latency;
        self.agent_activity = state.agent_activity;
    }

    fn remember_agent(&mut self, agent_id: String) {
        if let Ok(mut agents) = self.known_agents.lock()
            && !agents.iter().any(|known| known == &agent_id)
        {
            agents.push(agent_id);
            agents.sort();
        }
    }

    fn save_cli_state(&self) {
        let state = tau_config::settings::CliState {
            show_diff: self.diffs_expanded,
            show_thinking: self.show_thinking,
            show_turn_stats: self.show_turn_stats,
            redraw_counter: self.redraw_counter,
            show_tools: self.show_tools,
            show_messages: self.show_messages,
        };
        if let Ok(mut mirror) = self.cli_state_mirror.lock() {
            *mirror = state.clone();
        }
        state.save(&self.state_dirs);
    }

    /// Shared snapshot of the persisted CLI settings, updated in sync
    /// with every successful `/set` (i.e. on every
    /// [`Self::save_cli_state`] call). Cloned by the input loop so the
    /// `/set` name-completion menu can show each setting's current
    /// value without touching renderer-thread fields directly.
    pub(crate) fn cli_state_mirror(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_config::settings::CliState>> {
        self.cli_state_mirror.clone()
    }

    pub(crate) fn editor_context(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<tau_cli_term::EditorContext>> {
        self.editor_context.clone()
    }

    /// Returns a shared flag that is true while any agent/session work
    /// is in flight. The input loop uses it to keep Ctrl-D from
    /// terminating an active session accidentally.
    pub(crate) fn agent_in_progress_state(&self) -> Arc<AtomicBool> {
        self.agent_in_progress.clone()
    }

    /// Returns a clone of the shared Fast-mode mirror, used by configurable
    /// bindings.
    pub(crate) fn fast_service_tier_state(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.fast_service_tier_state.clone()
    }

    /// Returns a clone of the shared active-role mirror used by role cycling.
    pub(crate) fn current_role_state(&self) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
        self.current_role_state.clone()
    }

    /// Returns a clone of the shared ordered role list used by role cycling.
    pub(crate) fn roles_available(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        self.roles_available.clone()
    }

    /// Returns a clone of the shared ordered role groups used by role cycling.
    pub(crate) fn role_groups_available(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<tau_proto::HarnessRoleGroup>>> {
        self.role_groups_available.clone()
    }

    /// Returns a clone of the per-group runtime role memory used by role
    /// cycling.
    pub(crate) fn role_group_memory(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<HashMap<String, String>>> {
        self.role_group_memory.clone()
    }

    /// Apply a `/set <name> <value>` change. The caller (input loop)
    /// has already validated `name` and `value` against the
    /// [`crate::settings_registry`] table.
    pub(crate) fn apply_setting(&mut self, name: &str, value: &str) {
        let on = value == "true";
        match name {
            "show-diff" => self.set_diffs_expanded(on),
            "show-thinking" => self.set_show_thinking(on),
            "show-turn-stats" => self.set_show_turn_stats(on),
            "redraw-counter" => self.set_redraw_counter(on),
            "show-tools" => {
                if let Some(show_tools) = tau_config::settings::ShowTools::parse(value) {
                    self.set_show_tools(show_tools);
                }
            }
            "show-messages" => {
                if let Some(show_messages) = tau_config::settings::ShowMessages::parse(value) {
                    self.set_show_messages(show_messages);
                }
            }
            _ => {}
        }
    }

    /// Set the global expand-diffs flag and re-render every diff
    /// block in the chat history so the entire transcript switches
    /// mode at once. No-op if already in the requested state.
    fn set_diffs_expanded(&mut self, on: bool) {
        if self.diffs_expanded == on {
            return;
        }
        self.diffs_expanded = on;
        for entry in &self.diff_blocks {
            let block = self.render_diff_history_block(&entry.display, &entry.diff);
            self.handle.set_block(entry.block_id, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Set the global show-thinking flag and re-render every prior
    /// thinking block in the transcript so the change takes effect
    /// retroactively (full text when on, empty content when off).
    /// Live in-flight thinking blocks are also flipped. New turns
    /// continue to be gated by the same flag.
    ///
    /// Empty content is used instead of `remove_block` so the
    /// block's position in the transcript is preserved; turning
    /// back on restores the original reasoning text in place.
    fn set_show_thinking(&mut self, on: bool) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        if self.show_thinking == on {
            return;
        }
        self.show_thinking = on;
        for entry in &self.thinking_history {
            let display = if self.show_thinking {
                entry.text.as_str()
            } else {
                ""
            };
            self.handle.set_block(
                entry.block_id,
                themed_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        for state in self.prompts.values() {
            let Some(bid) = state.thinking_block_id else {
                continue;
            };
            let display = if self.show_thinking {
                state.thinking_text.clone().unwrap_or_default()
            } else {
                String::new()
            };
            self.handle.set_block(
                bid,
                themed_block(&self.theme, names::AGENT_THINKING, display),
            );
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Force a full repaint after a `/set show-*` change. Edited blocks
    /// from earlier in the transcript may already have scrolled out of
    /// the visible window, so the renderer needs to redraw from scratch
    /// for the change to take effect retroactively across scrollback.
    fn invalidate_for_retroactive_toggle(&mut self) {
        self.handle.invalidate_screen();
    }

    fn set_redraw_counter(&mut self, on: bool) {
        if self.redraw_counter == on {
            return;
        }
        self.redraw_counter = on;
        self.render_model_status();
        self.save_cli_state();
    }

    fn set_show_turn_stats(&mut self, on: bool) {
        if self.show_turn_stats == on {
            return;
        }
        self.show_turn_stats = on;
        for (index, entry) in self.turn_stats_history.iter().enumerate() {
            let previous_usage = index
                .checked_sub(1)
                .and_then(|previous_index| self.turn_stats_history.get(previous_index))
                .map(|previous_entry| &previous_entry.usage);
            let block = if self.show_turn_stats {
                render_turn_stats_block(
                    &self.theme,
                    &entry.usage,
                    previous_usage,
                    entry.turn_latency,
                    entry.total_latency,
                )
            } else {
                Self::empty_block()
            };
            self.handle.set_block(entry.block_id, block);
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    fn empty_block() -> tau_cli_term::StyledBlock {
        tau_cli_term::StyledBlock::new(tau_cli_term::StyledText::from(String::new()))
    }

    fn compaction_token_chip(tokens: u64) -> String {
        format!("#{}", format_token_count(tokens))
    }

    fn compaction_progress_status(original_input_tokens: Option<u64>) -> String {
        match original_input_tokens {
            Some(tokens) => format!(
                "{} {}",
                Self::compaction_token_chip(tokens),
                tau_proto::PROGRESS_INDICATOR_TEXT,
            ),
            None => tau_proto::PROGRESS_INDICATOR_TEXT.to_owned(),
        }
    }

    fn compaction_success_status(
        original_input_tokens: Option<u64>,
        compacted_input_tokens: Option<u64>,
    ) -> String {
        match (original_input_tokens, compacted_input_tokens) {
            (Some(original), Some(compacted)) => format!(
                "{} ok: {}",
                Self::compaction_token_chip(original),
                Self::compaction_token_chip(compacted)
            ),
            (Some(original), None) => format!("{} ok", Self::compaction_token_chip(original)),
            (None, Some(compacted)) => format!("ok: {}", Self::compaction_token_chip(compacted)),
            (None, None) => "ok".to_owned(),
        }
    }

    fn compaction_error_status(message: Option<&str>) -> String {
        let label = message
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("failed");
        let mut short: String = label.chars().take(48).collect();
        if label.chars().nth(48).is_some() {
            short.push('…');
        }
        format!("err: {short}")
    }

    fn compaction_failure_status(
        original_input_tokens: Option<u64>,
        message: Option<&str>,
    ) -> String {
        let error = Self::compaction_error_status(message);
        match original_input_tokens {
            Some(original) => format!("{} {error}", Self::compaction_token_chip(original)),
            None => error,
        }
    }

    fn handle_compaction_event(&mut self, event: &Event) -> bool {
        let originator = match event {
            Event::SessionCompactionStarted(started) => &started.originator,
            Event::SessionCompactionFinished(finished) => &finished.originator,
            Event::SessionCompacted(compacted) => &compacted.originator,
            _ => return false,
        };
        if !originator.is_user() {
            return true;
        }

        match event {
            Event::SessionCompactionStarted(started) => {
                if let Some(existing) = self.compaction_blocks.remove(&started.session_id) {
                    self.handle.remove_block(existing);
                }
                let block = render_compaction_block(
                    &self.theme,
                    Self::compaction_progress_status(started.original_input_tokens),
                    CompactionStatus::Progress,
                );
                let id = self.handle.new_block("compaction-progress", block);
                self.handle.push_above_active(id);
                self.handle.redraw();
                self.compaction_blocks
                    .insert(started.session_id.clone(), id);
                true
            }
            Event::SessionCompacted(compacted) => {
                // `SessionCompacted` is the durable success fact replayed to
                // late-joining UIs. During a live compaction we still wait for
                // `SessionCompactionFinished` to replace the in-flight block;
                // on replay there is no lifecycle block, so render the final
                // status from this event.
                if !self.compaction_blocks.contains_key(&compacted.session_id) {
                    self.handle.print_output(
                        "compaction-result",
                        render_compaction_block(
                            &self.theme,
                            Self::compaction_success_status(
                                compacted.original_input_tokens,
                                compacted.compacted_input_tokens,
                            ),
                            CompactionStatus::Success,
                        ),
                    );
                }
                true
            }
            Event::SessionCompactionFinished(finished) => {
                if let Some(block_id) = self.compaction_blocks.remove(&finished.session_id) {
                    self.handle.remove_block(block_id);
                }
                let (status_text, status) = match finished.outcome {
                    tau_proto::SessionCompactionOutcome::Succeeded => (
                        Self::compaction_success_status(
                            finished.original_input_tokens,
                            finished.compacted_input_tokens,
                        ),
                        CompactionStatus::Success,
                    ),
                    tau_proto::SessionCompactionOutcome::Failed => (
                        Self::compaction_failure_status(
                            finished.original_input_tokens,
                            finished.message.as_deref(),
                        ),
                        CompactionStatus::Error,
                    ),
                };
                self.handle.print_output(
                    "compaction-result",
                    render_compaction_block(&self.theme, status_text, status),
                );
                true
            }
            _ => false,
        }
    }

    fn render_tool_history_block(&self, display: &ToolCallDisplay) -> tau_cli_term::StyledBlock {
        match self.show_tools {
            tau_config::settings::ShowTools::Full => render_tool_block(&self.theme, display),
            tau_config::settings::ShowTools::Compact => self.render_compact_tool_block(display),
            tau_config::settings::ShowTools::Off
            | tau_config::settings::ShowTools::SummarizeTurn
            | tau_config::settings::ShowTools::SummarizePrompt => Self::empty_block(),
        }
    }

    fn render_compact_tool_block(&self, display: &ToolCallDisplay) -> tau_cli_term::StyledBlock {
        let mut display = display.clone();
        display.payload = None;
        render_tool_block(&self.theme, &display)
    }

    fn render_diff_history_block(
        &self,
        display: &ToolCallDisplay,
        diff: &tau_proto::DiffSummary,
    ) -> tau_cli_term::StyledBlock {
        match self.show_tools {
            tau_config::settings::ShowTools::Full => {
                render_diff_tool_block(&self.theme, display, diff, self.diffs_expanded)
            }
            tau_config::settings::ShowTools::Compact => self.render_compact_tool_block(display),
            tau_config::settings::ShowTools::Off
            | tau_config::settings::ShowTools::SummarizeTurn
            | tau_config::settings::ShowTools::SummarizePrompt => Self::empty_block(),
        }
    }

    fn render_summary_block(&self, summary: &ToolSummaryDisplay) -> tau_cli_term::StyledBlock {
        if matches!(
            self.show_tools,
            tau_config::settings::ShowTools::SummarizeTurn
                | tau_config::settings::ShowTools::SummarizePrompt
        ) {
            render_tool_block(&self.theme, &build_tool_summary_display(summary))
        } else {
            Self::empty_block()
        }
    }

    fn update_tool_summary_block(&mut self, block_id: tau_cli_term::BlockId) {
        let Some(summary) = self.tool_summaries.get(&block_id) else {
            return;
        };
        self.handle
            .set_block(block_id, self.render_summary_block(summary));
    }

    fn record_tool_summary_result(
        &mut self,
        block_id: Option<tau_cli_term::BlockId>,
        display: Option<&tau_proto::ToolDisplay>,
        diff: Option<&tau_proto::DiffSummary>,
        is_error: bool,
    ) {
        let Some(block_id) = block_id else {
            return;
        };
        if let Some(summary) = self.tool_summaries.get_mut(&block_id) {
            summary.completed += 1;
            if is_error {
                summary.err += 1;
            } else {
                summary.ok += 1;
            }
            if let Some(display) = display {
                summary.matches += display.stats.matches.unwrap_or(0);
                summary.lines += display.stats.lines.unwrap_or(0);
                summary.bytes += display.stats.bytes.unwrap_or(0);
            }
            if let Some(diff) = diff {
                summary.added += u64::from(diff.added);
                summary.removed += u64::from(diff.removed);
            }
        }
        let finished = self
            .tool_summaries
            .get(&block_id)
            .is_some_and(|summary| summary.completed == summary.total);
        if finished {
            if self.prompt_tool_summary == Some(block_id) && self.prompt_tool_summary_active {
                self.update_tool_summary_block(block_id);
                return;
            }
            let Some(summary) = self.tool_summaries.remove(&block_id) else {
                return;
            };
            self.handle.remove_block(block_id);
            let new_block_id = self
                .handle
                .print_output("tool-summary", self.render_summary_block(&summary));
            self.tool_summaries.insert(new_block_id, summary);
        } else {
            self.update_tool_summary_block(block_id);
        }
    }

    fn set_show_messages(&mut self, show_messages: tau_config::settings::ShowMessages) {
        if self.show_messages == show_messages {
            return;
        }
        self.show_messages = show_messages;
        for entry in &self.message_history {
            self.handle.set_block(
                entry.block_id,
                self.render_agent_message_block(&entry.message),
            );
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    fn set_show_tools(&mut self, show_tools: tau_config::settings::ShowTools) {
        if self.show_tools == show_tools {
            return;
        }
        self.show_tools = show_tools;
        for entry in &self.tool_history {
            self.handle.set_block(
                entry.block_id,
                self.render_tool_history_block(&entry.display),
            );
        }
        for entry in &self.diff_blocks {
            self.handle.set_block(
                entry.block_id,
                self.render_diff_history_block(&entry.display, &entry.diff),
            );
        }
        for (block_id, summary) in &self.tool_summaries {
            self.handle
                .set_block(*block_id, self.render_summary_block(summary));
        }
        for state in self.tool_calls.values() {
            if let Some(block_id) = state.block_id {
                let block = state
                    .live_display
                    .as_ref()
                    .map(|display| self.render_tool_history_block(display))
                    .unwrap_or_else(Self::empty_block);
                self.handle.set_block(block_id, block);
            }
            if let Some(block_id) = state.summary_block_id
                && let Some(summary) = self.tool_summaries.get(&block_id)
            {
                self.handle
                    .set_block(block_id, self.render_summary_block(summary));
            }
        }
        self.invalidate_for_retroactive_toggle();
        self.save_cli_state();
    }

    /// Clears all session-scoped UI state and re-renders an empty
    /// transcript. Persistent user preferences such as `show-diff`
    /// and `show-thinking` are intentionally preserved.
    fn clear_for_new_session(&mut self) {
        self.prompts.clear();
        self.compaction_blocks.clear();
        self.last_user_block = None;
        self.queued_user_blocks.clear();
        self.tool_calls.clear();
        if let Some(timer) = &self.tool_timer {
            timer.clear_active();
        }
        self.shell_blocks.clear();
        self.extension_blocks.clear();
        self.model_status_block = None;
        self.diff_blocks.clear();
        self.thinking_history.clear();
        self.turn_stats_history.clear();
        self.tool_history.clear();
        self.message_history.clear();
        self.tool_summaries.clear();
        self.prompt_tool_summary = None;
        self.prompt_tool_summary_active = false;
        // Model selection and effort are harness-global, not
        // session-scoped. `/new` only causes a SessionStarted event;
        // the harness does not re-emit HarnessRoleSelected for the
        // unchanged model. Keep the cached selection so the status bar
        // can be recreated after clearing the terminal output.
        self.current_context_percent = None;
        self.current_context_input_tokens = None;
        self.main_tools_completed = 0;
        self.main_tools_total = 0;
        self.main_backgrounded_tools.clear();
        self.main_agent_turn_active = false;
        self.main_tools_visible = false;
        self.cumulative_agent_latency = Duration::ZERO;
        self.handle.clear_output();
        self.render_session_preamble();
        if self.current_model.is_some() || self.current_role.is_some() {
            self.render_model_status();
        }
    }

    fn render_session_preamble(&mut self) {
        self.handle.print_output(
            "banner",
            tau_cli_term::StyledBlock::new(build_banner(&self.theme)),
        );
        let mut extensions: Vec<_> = self.ready_extensions.iter().collect();
        extensions.sort();
        for extension_name in extensions {
            self.handle.print_output(
                "extension-kept",
                extension_status_block(&self.theme, extension_name, "kept"),
            );
        }
    }

    fn render_model_status(&mut self) {
        use tau_cli_term::StyledBlock;
        use tau_cli_term::resolve::{convert_color, themed_text};
        use tau_themes::{StyleName, ThemedText, names};

        let mut themed = ThemedText::new();
        let mut right_themed = ThemedText::new();
        let status_style = themed.add_style(names::MODEL_STATUS);
        let model_style = themed.add_style(names::STATUS_MODEL);
        let role_style = themed.add_style(names::STATUS_ROLE);
        let session_style = themed.add_style(names::STATUS_SESSION);
        let effort_style = themed.add_style(names::STATUS_EFFORT);
        let verbosity_style = themed.add_style(names::STATUS_VERBOSITY);
        let service_tier_style = themed.add_style(names::STATUS_SERVICE_TIER);
        let tools_style = right_themed.add_style(names::STATUS_TOOLS);
        let context_style = right_themed.add_style(names::STATUS_CONTEXT);
        let redraw_style = right_themed.add_style(names::REDRAW_COUNTER);
        let mut needs_space = false;
        let mut right_needs_space = false;

        match (self.current_role.as_deref(), self.current_model.as_ref()) {
            (Some(role), _) => push_status_chip(
                &mut themed,
                role_style,
                &mut needs_space,
                format!("+{role}"),
            ),
            (None, Some(model)) => push_status_chip(
                &mut themed,
                model_style,
                &mut needs_space,
                format!("={model}"),
            ),
            (None, None) => push_status_chip(
                &mut themed,
                status_style,
                &mut needs_space,
                "no role selected".to_owned(),
            ),
        }
        let show_effort = self.baseline_params.map_or_else(
            || {
                self.role_default_effort()
                    .map_or(!self.model_params.effort.is_default(), |default| {
                        self.model_params.effort != default
                    })
            },
            |default| self.model_params.effort != default.effort,
        );
        if show_effort {
            push_status_chip(
                &mut themed,
                effort_style,
                &mut needs_space,
                format!("^{}", self.model_params.effort.as_str()),
            );
        }
        let show_verbosity = self.baseline_params.map_or_else(
            || {
                self.role_default_verbosity()
                    .map_or(!self.model_params.verbosity.is_default(), |default| {
                        self.model_params.verbosity != default
                    })
            },
            |default| self.model_params.verbosity != default.verbosity,
        );
        if show_verbosity {
            push_status_chip(
                &mut themed,
                verbosity_style,
                &mut needs_space,
                format!("~{}", self.model_params.verbosity.as_str()),
            );
        }
        let show_service_tier = self
            .baseline_params
            .map_or(self.model_params.service_tier.is_some(), |default| {
                self.model_params.service_tier != default.service_tier
            });
        if show_service_tier {
            let service_tier = self
                .model_params
                .service_tier
                .map(|tier| tier.as_str())
                .unwrap_or("off");
            push_status_chip(
                &mut themed,
                service_tier_style,
                &mut needs_space,
                format!("!{service_tier}"),
            );
        }
        if let Some(agent_id) = self
            .current_agent_id
            .as_deref()
            .filter(|agent_id| *agent_id != MAIN_AGENT_ID)
        {
            push_status_chip(
                &mut themed,
                session_style,
                &mut needs_space,
                format!("&{agent_id}"),
            );
        }
        if let Some(session_id) = self.current_session_id.as_ref() {
            push_status_chip(
                &mut themed,
                session_style,
                &mut needs_space,
                format!("@{session_id}"),
            );
        }
        if let Some(tools) = self.main_tools_status_chip() {
            push_status_chip(
                &mut right_themed,
                tools_style,
                &mut right_needs_space,
                format!("%{tools}"),
            );
        }
        if let Some(context) = self.context_status_chip() {
            push_status_chip(
                &mut right_themed,
                context_style,
                &mut right_needs_space,
                format!("#{context}"),
            );
        }

        let full_render_count = self.handle.full_render_count();
        if self.last_full_render_count < full_render_count {
            self.last_full_render_count = full_render_count;
            self.last_full_render_at = Some(Instant::now());
        }
        let show_redraw_counter = self.redraw_counter
            && self
                .last_full_render_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(5 * 60));
        if show_redraw_counter {
            push_status_chip(
                &mut right_themed,
                redraw_style,
                &mut right_needs_space,
                full_render_count.to_string(),
            );
        }

        let bg = self
            .theme
            .resolve_style(&StyleName::new(names::MODEL_STATUS))
            .bg;
        let mut block = StyledBlock::new(themed_text(&self.theme, &themed))
            .right_content(themed_text(&self.theme, &right_themed));
        if let Some(bg) = bg {
            block = block.bg(convert_color(bg));
        }
        match self.model_status_block {
            Some(bid) => {
                self.handle.set_block(bid, block);
            }
            None => {
                let bid = self.handle.new_block("model-status", block);
                self.handle.push_below(bid);
                self.model_status_block = Some(bid);
            }
        }
        self.handle.redraw();
    }

    fn role_default_effort(&self) -> Option<tau_proto::Effort> {
        let role = self.current_role.as_deref()?;
        self.role_defaults
            .get(role)?
            .effort
            .as_deref()?
            .parse()
            .ok()
    }

    fn role_default_verbosity(&self) -> Option<tau_proto::Verbosity> {
        let role = self.current_role.as_deref()?;
        self.role_defaults
            .get(role)?
            .verbosity
            .as_deref()?
            .parse()
            .ok()
    }

    fn main_tools_status_chip(&self) -> Option<String> {
        self.live_main_delegate_tools_status_chip().or_else(|| {
            ((self.main_tools_visible || !self.main_backgrounded_tools.is_empty())
                && self.main_tools_total != 0)
                .then(|| format!("{}/{}", self.main_tools_completed, self.main_tools_total))
        })
    }

    fn live_main_delegate_tools_status_chip(&self) -> Option<String> {
        self.tool_calls
            .values()
            .filter(|state| {
                state.is_main_delegate && !state.is_sub_agent && state.block_id.is_some()
            })
            .filter_map(|state| state.delegate_last_progress.as_ref())
            .find_map(Self::delegate_progress_tools_status_chip)
    }

    fn delegate_progress_tools_status_chip(
        progress: &tau_proto::DelegateProgress,
    ) -> Option<String> {
        progress
            .display
            .as_ref()
            .and_then(|display| {
                display
                    .progress_counters
                    .iter()
                    .find_map(Self::tools_progress_counter_status_chip)
            })
            .or_else(|| {
                (progress.tools_total != 0).then(|| {
                    format!(
                        "{}/{}",
                        progress
                            .tools_total
                            .saturating_sub(progress.tools_in_flight),
                        progress.tools_total
                    )
                })
            })
    }

    fn tools_progress_counter_status_chip(counter: &tau_proto::ProgressCounter) -> Option<String> {
        if counter.label.as_deref() != Some("tools")
            || counter.unit != tau_proto::ProgressUnit::Count
        {
            return None;
        }
        Some(match (counter.complete, counter.total) {
            (Some(complete), Some(total)) => format!("{complete}/{total}"),
            (Some(complete), None) => complete.to_string(),
            (None, Some(total)) => format!("?/{total}"),
            (None, None) => "?".to_owned(),
        })
    }

    fn record_main_tool_completed(&mut self) {
        if self.main_tools_completed < self.main_tools_total {
            self.main_tools_completed += 1;
        }
    }

    fn set_main_tools_visible(&mut self, visible: bool) {
        if self.main_tools_visible == visible {
            return;
        }
        self.main_tools_visible = visible;
        if self.model_status_block.is_some() {
            self.render_model_status();
        }
    }

    fn set_main_agent_turn_active(&mut self, active: bool) {
        self.main_agent_turn_active = active;
        self.set_main_tools_visible(active && self.main_tools_total != 0);
    }

    fn has_live_main_delegate_tool_call(&self) -> bool {
        self.tool_calls
            .values()
            .any(|state| state.is_main_delegate && !state.is_sub_agent)
    }

    fn sync_agent_activity_for_lifecycle(&mut self, event: &Event) {
        match event {
            Event::UiPromptSubmitted(_) => self.agent_activity.mark_optimistic_submission(),
            Event::SessionPromptCreated(prompt) => {
                self.agent_activity.start_prompt(&prompt.session_prompt_id);
            }
            Event::ProviderPromptSubmitted(submitted) => {
                self.agent_activity
                    .start_prompt(&submitted.session_prompt_id);
            }
            Event::ProviderResponseUpdated(update) => {
                self.agent_activity.start_prompt(&update.session_prompt_id);
            }
            Event::ProviderResponseFinished(finished) => {
                self.agent_activity
                    .finish_prompt(&finished.session_prompt_id, &finished.output_items);
            }
            Event::SessionPromptTerminated(terminated) => {
                self.agent_activity
                    .finish_prompt(&terminated.session_prompt_id, &[]);
            }
            Event::ToolRequest(_) => {}
            Event::ToolStarted(invoke) => self.agent_activity.start_tool(&invoke.call_id),
            Event::ToolRejected(rejected) => {
                self.agent_activity.finish_tool(&rejected.call_id);
            }
            Event::ToolResult(result) | Event::ProviderToolResult(result) => {
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
                    self.agent_activity.background_tool(&result.call_id);
                } else {
                    self.agent_activity.finish_tool(&result.call_id);
                }
            }
            Event::ToolError(error) | Event::ProviderToolError(error) => {
                self.agent_activity.finish_tool(&error.call_id);
            }
            Event::ToolBackgroundResult(result) => {
                self.agent_activity.finish_background_tool(&result.call_id);
            }
            Event::ToolBackgroundError(error) => {
                self.agent_activity.finish_background_tool(&error.call_id);
            }
            Event::ToolCancelled(cancelled) => {
                self.agent_activity
                    .finish_background_tool(&cancelled.call_id);
            }
            Event::UiCancelPrompt(_) => self.agent_activity.clear_optimistic_submissions(),
            Event::SessionShutdown(_) => self.agent_activity.clear(),
            _ => {}
        }
        self.agent_in_progress
            .store(self.agent_activity.is_in_progress(), Ordering::Relaxed);
    }

    fn sync_main_tools_visibility_for_prompt_lifecycle(&mut self, event: &Event) {
        match event {
            Event::SessionPromptCreated(prompt) => {
                if prompt.originator.is_user() || !self.has_live_main_delegate_tool_call() {
                    self.set_main_agent_turn_active(prompt.originator.is_user());
                }
            }
            Event::ProviderPromptSubmitted(submitted) => {
                if submitted.originator.is_user() || !self.has_live_main_delegate_tool_call() {
                    self.set_main_agent_turn_active(submitted.originator.is_user());
                }
            }
            Event::ProviderResponseUpdated(update) => {
                if update.originator.is_user() || !self.has_live_main_delegate_tool_call() {
                    self.set_main_agent_turn_active(update.originator.is_user());
                }
            }
            Event::ProviderResponseFinished(finished)
                if !finished.originator.is_user() && !self.has_live_main_delegate_tool_call() =>
            {
                self.set_main_agent_turn_active(false);
            }
            Event::SessionPromptTerminated(terminated) if terminated.originator.is_user() => {
                if self.agent_activity.active_prompts.is_empty() {
                    self.set_main_agent_turn_active(false);
                }
            }
            Event::SessionPromptTerminated(terminated)
                if !terminated.originator.is_user() && !self.has_live_main_delegate_tool_call() =>
            {
                self.set_main_agent_turn_active(false);
            }
            _ => {}
        }
    }

    fn reset_main_tool_usage(&mut self) {
        if self.main_tools_completed == 0
            && self.main_tools_total == 0
            && !self.main_tools_visible
            && self.main_backgrounded_tools.is_empty()
        {
            return;
        }
        if self.main_backgrounded_tools.is_empty() {
            self.main_tools_completed = 0;
            self.main_tools_total = 0;
            self.main_tools_visible = false;
        } else {
            self.main_tools_visible = true;
        }
        if self.model_status_block.is_some() {
            self.render_model_status();
        }
    }

    fn context_status_chip(&self) -> Option<String> {
        match (
            self.current_context_percent,
            self.current_context_input_tokens,
            self.current_context_window,
        ) {
            (_, Some(input), Some(window)) => Some(format!(
                "{}/{}",
                format_token_count(input),
                format_token_count(window)
            )),
            (Some(percent), _, Some(window)) => {
                Some(format!("{percent}%/{}", format_token_count(window)))
            }
            (Some(percent), _, None) => Some(format!("{percent}%")),
            (None, Some(input), None) => Some(format_token_count(input)),
            (None, None, Some(window)) => Some(format!("?/{}", format_token_count(window))),
            (None, None, None) => None,
        }
    }

    fn submitted_prompt_block(
        &self,
        body_name: &str,
        body_text: impl Into<String>,
    ) -> tau_cli_term::StyledBlock {
        use tau_cli_term::resolve::{convert_color, themed_text};
        use tau_themes::{SpanTree, StyleName, ThemedText, names};

        let mut themed = ThemedText::new();
        let body_style = themed.add_style(body_name);
        let marker_style = themed.add_style(names::PROMPT_MARKER_SUBMITTED);
        themed.push_tree(SpanTree::span(
            body_style,
            vec![
                SpanTree::span(
                    marker_style,
                    vec![SpanTree::text(format!("{} ", self.submitted_prompt_symbol))],
                ),
                SpanTree::text(body_text.into()),
            ],
        ));

        let body_ts = self.theme.resolve_style(&StyleName::new(body_name));
        let mut block = tau_cli_term::StyledBlock::new(themed_text(&self.theme, &themed));
        if let Some(bg) = body_ts.bg {
            block = block.bg(convert_color(bg));
        }
        block
    }

    pub(crate) fn handle_disconnect(&mut self, reason: Option<String>) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;
        self.agent_activity.clear();
        self.agent_in_progress.store(false, Ordering::Relaxed);
        let mut summary_blocks = HashSet::new();
        for state in self.tool_calls.values() {
            if let Some(block_id) = state.block_id {
                self.handle.remove_block(block_id);
            }
            if let Some(block_id) = state.summary_block_id {
                summary_blocks.insert(block_id);
            }
        }
        for block_id in summary_blocks {
            self.handle.remove_block(block_id);
            self.tool_summaries.remove(&block_id);
            if self.prompt_tool_summary == Some(block_id) {
                self.prompt_tool_summary = None;
                self.prompt_tool_summary_active = false;
            }
        }
        if self.prompt_tool_summary_active {
            self.finish_prompt_tool_summary();
        }
        self.tool_calls.clear();
        if let Some(timer) = &self.tool_timer {
            timer.clear_active();
        }
        let reason = reason.as_deref().unwrap_or("disconnected");
        self.handle.print_output(
            "system-disconnect",
            themed_block(&self.theme, names::SYSTEM_DISCONNECT, reason),
        );
    }

    fn learn_side_conversation_tool_calls(&mut self, event: &Event) {
        let Event::ProviderResponseFinished(finished) = event else {
            return;
        };
        if finished.originator.is_user() {
            return;
        }
        for call in tool_calls_from_output_items(&finished.output_items) {
            self.tool_calls.insert(
                call.call_id.to_string(),
                ToolCallState {
                    is_sub_agent: true,
                    ..ToolCallState::default()
                },
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn handle(&mut self, event: &Event) {
        self.handle_recorded_at(event, UnixMicros::now());
    }

    pub(crate) fn handle_recorded_at(&mut self, event: &Event, recorded_at: UnixMicros) {
        self.learn_agent_metadata(event);
        let target_agent_id = self.agent_id_for_event(event);
        if self.current_agent_id.is_none() {
            self.current_agent_id = Some(target_agent_id.clone());
            if let Ok(mut current) = self.current_agent_state.lock() {
                *current = Some(target_agent_id.clone());
            }
            self.handle_recorded_at_for_visible_agent(event, recorded_at);
            return;
        }
        if self.current_agent_id.as_deref() == Some(target_agent_id.as_str()) {
            self.handle_recorded_at_for_visible_agent(event, recorded_at);
            return;
        }

        let visible_agent_id = self
            .current_agent_id
            .clone()
            .unwrap_or_else(|| MAIN_AGENT_ID.to_owned());
        let visible_state = self.take_visible_agent_state();
        self.agents_ui_state
            .insert(visible_agent_id.clone(), visible_state);
        let target_state = self
            .agents_ui_state
            .remove(&target_agent_id)
            .unwrap_or_default();
        let handle = self.handle.clone();
        handle.with_redraw_suppressed(|| {
            self.restore_hidden_agent_state(target_state);
            self.current_agent_id = Some(target_agent_id.clone());
            self.handle_recorded_at_for_visible_agent(event, recorded_at);
            let target_state = self.take_visible_agent_state();
            self.agents_ui_state.insert(target_agent_id, target_state);
            let visible_state = self
                .agents_ui_state
                .remove(&visible_agent_id)
                .unwrap_or_default();
            self.restore_hidden_agent_state(visible_state);
        });
        self.current_agent_id = Some(visible_agent_id);
    }

    fn learn_agent_metadata(&mut self, event: &Event) {
        match event {
            Event::StartAgentRequest(request) => {
                self.query_agents
                    .insert(request.query_id.clone(), request.agent_id.clone());
                self.remember_agent(request.agent_id.clone());
            }
            Event::StartAgentAccepted(accepted) => {
                self.query_agents
                    .insert(accepted.query_id.clone(), accepted.agent_id.clone());
                self.remember_agent(accepted.agent_id.clone());
            }
            Event::SessionPromptCreated(prompt) => {
                let agent_id = self.agent_id_for_originator(&prompt.originator);
                self.prompt_agents
                    .insert(prompt.session_prompt_id.to_string(), agent_id);
            }
            Event::ProviderResponseFinished(finished) => {
                let agent_id = self.agent_id_for_originator(&finished.originator);
                self.prompt_agents
                    .insert(finished.session_prompt_id.to_string(), agent_id.clone());
                for call in tool_calls_from_output_items(&finished.output_items) {
                    self.tool_agents
                        .insert(call.call_id.to_string(), agent_id.clone());
                }
            }
            Event::AgentMessage(message) => {
                if message.sender_id != "user" {
                    self.remember_agent(message.sender_id.clone());
                }
                if message.recipient_id != "user" {
                    self.remember_agent(message.recipient_id.clone());
                }
            }
            _ => {}
        }
    }

    fn agent_id_for_event(&self, event: &Event) -> String {
        match event {
            Event::ToolRequest(request) => self
                .tool_agents
                .get(request.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| MAIN_AGENT_ID.to_owned()),
            Event::ToolStarted(started) => self
                .tool_agents
                .get(started.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| MAIN_AGENT_ID.to_owned()),
            Event::ToolProgress(progress) => self
                .tool_agents
                .get(progress.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| MAIN_AGENT_ID.to_owned()),
            Event::ToolResult(result) | Event::ProviderToolResult(result) => self
                .tool_agents
                .get(result.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| self.agent_id_for_originator(&result.originator)),
            Event::ToolError(error) | Event::ProviderToolError(error) => self
                .tool_agents
                .get(error.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| self.agent_id_for_originator(&error.originator)),
            Event::ToolBackgroundResult(result) => self
                .tool_agents
                .get(result.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| MAIN_AGENT_ID.to_owned()),
            Event::ToolBackgroundError(error) => self
                .tool_agents
                .get(error.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| MAIN_AGENT_ID.to_owned()),
            Event::ToolCancelled(cancelled) => self
                .tool_agents
                .get(cancelled.call_id.as_str())
                .cloned()
                .unwrap_or_else(|| MAIN_AGENT_ID.to_owned()),
            Event::AgentMessage(message) if message.recipient_id == "user" => {
                message.sender_id.clone()
            }
            Event::AgentMessage(message) if message.sender_id == "user" => {
                message.recipient_id.clone()
            }
            _ => self.agent_id_for_originator(&originator_of(event)),
        }
    }

    fn agent_id_for_originator(&self, originator: &tau_proto::PromptOriginator) -> String {
        match originator {
            tau_proto::PromptOriginator::User => MAIN_AGENT_ID.to_owned(),
            tau_proto::PromptOriginator::Extension { query_id, .. } => self
                .query_agents
                .get(query_id)
                .cloned()
                .unwrap_or_else(|| query_id.clone()),
        }
    }

    fn handle_recorded_at_for_visible_agent(&mut self, event: &Event, recorded_at: UnixMicros) {
        self.sync_agent_activity_for_lifecycle(event);

        if self.handle_compaction_event(event) {
            return;
        }

        self.sync_main_tools_visibility_for_prompt_lifecycle(event);

        // Side-conversation `ProviderResponseFinished` events get filtered
        // out by `originator_of(event).is_user()` below — but we still
        // need to learn which `call_id`s those side conversations emit,
        // so later `ToolResult` / `ToolError` / `ToolProgress` events
        // (which carry no originator) can be suppressed before they leak
        // into the user's transcript.
        self.learn_side_conversation_tool_calls(event);

        if self.handle_agent_message_event(event) {
            return;
        }

        // Events are routed to the owning agent transcript before reaching this
        // point, so side-conversation events are rendered into their own hidden
        // or visible state instead of being dropped.

        if self.handle_session_events(event)
            || self.handle_prompt_events(event)
            || self.handle_provider_response_events(event)
            || self.handle_tool_events(event, recorded_at)
            || self.handle_shell_events(event)
            || self.handle_action_events(event)
            || self.handle_extension_events(event)
            || self.handle_harness_status_events(event)
            || self.handle_harness_role_events(event)
            || self.handle_harness_available_events(event)
            || self.handle_terminal_events(event)
        {
            return;
        }

        Self::trace_unhandled_event(event);
    }

    fn trace_unhandled_event(event: &Event) {
        tracing::trace!(
            target: "tau_cli::ui",
            event = ?std::mem::discriminant(event),
            "unhandled event variant"
        );
    }

    fn handle_agent_message_event(&mut self, event: &Event) -> bool {
        let Event::AgentMessage(message) = event else {
            return false;
        };
        let block = self.render_agent_message_block(message);
        let block_id = self.handle.print_output("agent-message", block);
        self.message_history.push(MessageBlockEntry {
            block_id,
            message: message.clone(),
        });
        true
    }

    fn render_agent_message_block(
        &self,
        message: &tau_proto::AgentMessage,
    ) -> tau_cli_term::StyledBlock {
        match Self::message_render_mode(self.show_messages, message) {
            MessageRenderMode::Hidden => Self::empty_block(),
            MessageRenderMode::Summary => self.submitted_prompt_block(
                tau_themes::names::SYSTEM_INFO,
                Self::agent_message_summary(message),
            ),
            MessageRenderMode::Full => self.submitted_prompt_block(
                tau_themes::names::SYSTEM_INFO,
                format!(
                    "{}:\n{}",
                    Self::agent_message_summary(message),
                    message.message
                ),
            ),
        }
    }

    fn agent_message_summary(message: &tau_proto::AgentMessage) -> String {
        format!(
            "Message from {} to {}",
            message.sender_id, message.recipient_id
        )
    }

    fn message_render_mode(
        show_messages: tau_config::settings::ShowMessages,
        message: &tau_proto::AgentMessage,
    ) -> MessageRenderMode {
        let self_msg = message.sender_id == "user" || message.recipient_id == "user";
        match (show_messages, self_msg) {
            (tau_config::settings::ShowMessages::None, _) => MessageRenderMode::Hidden,
            (tau_config::settings::ShowMessages::SelfSummary, true) => MessageRenderMode::Summary,
            (tau_config::settings::ShowMessages::SelfSummary, false) => MessageRenderMode::Hidden,
            (tau_config::settings::ShowMessages::SelfFull, true) => MessageRenderMode::Full,
            (tau_config::settings::ShowMessages::SelfFull, false) => MessageRenderMode::Hidden,
            (tau_config::settings::ShowMessages::AllSummary, true) => MessageRenderMode::Full,
            (tau_config::settings::ShowMessages::AllSummary, false) => MessageRenderMode::Summary,
            (tau_config::settings::ShowMessages::AllFull, _) => MessageRenderMode::Full,
        }
    }

    fn handle_session_events(&mut self, event: &Event) -> bool {
        match event {
            Event::SessionStarted(started)
                if matches!(started.reason, tau_proto::SessionStartReason::New) =>
            {
                self.handle_new_session_started(started);
                true
            }
            Event::SessionStarted(started) => {
                self.handle_existing_session_started(started);
                true
            }
            _ => false,
        }
    }

    fn handle_new_session_started(&mut self, started: &tau_proto::SessionStarted) {
        self.current_session_id = Some(started.session_id.clone());
        self.clear_for_new_session();
    }

    fn handle_existing_session_started(&mut self, started: &tau_proto::SessionStarted) {
        self.current_session_id = Some(started.session_id.clone());
        if self.model_status_block.is_some()
            || self.current_model.is_some()
            || self.current_role.is_some()
        {
            self.render_model_status();
        }
    }

    fn handle_prompt_events(&mut self, event: &Event) -> bool {
        match event {
            Event::UiPromptSubmitted(prompt) => {
                self.handle_ui_prompt_submitted(prompt);
                true
            }
            Event::SessionPromptQueued(queued) => {
                self.handle_session_prompt_queued(queued);
                true
            }
            Event::SessionPromptRecalled(recalled) => {
                self.handle_session_prompt_recalled(recalled);
                true
            }
            Event::SessionPromptSteered(steered) => {
                self.handle_session_prompt_steered(steered);
                true
            }
            Event::SessionPromptCreated(prompt) => {
                self.handle_session_prompt_created(prompt);
                true
            }
            Event::SessionPromptTerminated(terminated) => {
                self.handle_session_prompt_terminated(terminated);
                true
            }
            _ => false,
        }
    }

    fn handle_ui_prompt_submitted(&mut self, prompt: &tau_proto::UiPromptSubmitted) {
        if prompt.message_class.is_internal() {
            return;
        }

        use tau_themes::names;

        if self
            .queued_user_blocks
            .front()
            .is_some_and(|(_, text)| text == &prompt.text)
        {
            return;
        }
        self.reset_main_tool_usage();
        let block = self.submitted_prompt_block(names::USER_PROMPT, prompt.text.clone());
        let id = self.handle.print_output("user-prompt", block);
        self.last_user_block = Some((id, prompt.text.clone()));
    }

    fn handle_session_prompt_queued(&mut self, queued: &tau_proto::SessionPromptQueued) {
        if queued.message_class.is_internal() {
            return;
        }

        use tau_themes::names;

        self.reset_main_tool_usage();
        if let Some((id, text)) = self.last_user_block.take() {
            if text == queued.text {
                self.handle.remove_block(id);
            } else {
                self.last_user_block = Some((id, text));
            }
        }
        let block = self.submitted_prompt_block(
            names::USER_PROMPT_QUEUED,
            format!("{} (queued)", queued.text),
        );
        let queued_id = self.handle.new_block("user-prompt-queued", block);
        self.handle.push_above_sticky(queued_id);
        self.handle.redraw();
        self.queued_user_blocks
            .push_back((queued_id, queued.text.clone()));
    }

    fn handle_session_prompt_recalled(&mut self, recalled: &tau_proto::SessionPromptRecalled) {
        if let Some((queued_id, _text)) = self.queued_user_blocks.pop_back() {
            self.handle.remove_above_sticky(queued_id);
            self.handle.remove_block(queued_id);
        }
        self.handle
            .recall_prompt_before_current(recalled.text.clone());
        self.handle.redraw();
    }

    fn handle_session_prompt_steered(&mut self, steered: &tau_proto::SessionPromptSteered) {
        if steered.message_class.is_internal() {
            return;
        }

        use tau_themes::names;

        // The harness folded a queued prompt into the current turn's next
        // round (alongside tool results) instead of waiting for `Idle`.
        // Promote the "(queued)" rendering to a regular user prompt so the
        // transcript reads naturally above the agent's continuing response.
        if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
            self.handle.remove_block(queued_id);
            self.handle.print_output(
                "user-prompt-steered",
                self.submitted_prompt_block(names::USER_PROMPT, text),
            );
            self.handle.redraw();
        } else {
            // No matching "(queued)" block — fall back to rendering the
            // steered text directly so the user still sees their message land.
            self.handle.print_output(
                "user-prompt-steered",
                self.submitted_prompt_block(names::USER_PROMPT, steered.text.clone()),
            );
            self.handle.redraw();
        }
    }

    fn handle_session_prompt_created(&mut self, prompt: &tau_proto::SessionPromptCreated) {
        self.clear_editor_active_prompt_for_user_prompt(prompt.originator.is_user());
        self.last_user_block = None;
        self.prompts
            .entry(prompt.session_prompt_id.to_string())
            .or_default()
            .started_at = Some(Instant::now());
        self.promote_next_queued_prompt("user-prompt-created");
        self.create_live_response_block(prompt);
    }

    fn clear_editor_active_prompt_for_user_prompt(&mut self, is_user_prompt: bool) {
        if is_user_prompt && let Ok(mut context) = self.editor_context.lock() {
            context.active_prompt = None;
        }
    }

    fn handle_session_prompt_terminated(
        &mut self,
        terminated: &tau_proto::SessionPromptTerminated,
    ) {
        self.clear_editor_active_prompt_for_user_prompt(terminated.originator.is_user());
        let Some(prompt_state) = self.prompts.remove(terminated.session_prompt_id.as_str()) else {
            return;
        };
        if let Some(block_id) = prompt_state.thinking_block_id {
            self.handle.remove_block(block_id);
        }
        if let Some(block_id) = prompt_state.response_block_id {
            self.handle.remove_block(block_id);
        }
        self.handle.redraw();
    }

    fn promote_next_queued_prompt(&mut self, label: &'static str) {
        use tau_themes::names;

        if let Some((queued_id, text)) = self.queued_user_blocks.pop_front() {
            self.handle.remove_block(queued_id);
            self.handle
                .print_output(label, self.submitted_prompt_block(names::USER_PROMPT, text));
        }
    }

    fn create_live_response_block(&mut self, prompt: &tau_proto::SessionPromptCreated) {
        use tau_themes::names;

        let block = streaming_block(&self.theme, names::AGENT_PENDING, "");
        let id = self.handle.new_block(
            format!("agent-response-live:{}", prompt.session_prompt_id),
            block,
        );
        self.handle.push_above_active(id);
        self.handle.redraw();
        self.prompts
            .entry(prompt.session_prompt_id.to_string())
            .or_default()
            .response_block_id = Some(id);
    }

    fn handle_provider_response_events(&mut self, event: &Event) -> bool {
        match event {
            Event::ProviderPromptSubmitted(submitted) => {
                self.handle_provider_prompt_submitted(submitted);
                true
            }
            Event::ProviderResponseUpdated(update) => {
                self.handle_provider_response_updated(update);
                true
            }
            Event::ProviderResponseFinished(finished) => {
                self.handle_provider_response_finished(finished);
                true
            }
            _ => false,
        }
    }

    fn handle_provider_prompt_submitted(&mut self, submitted: &tau_proto::ProviderPromptSubmitted) {
        self.prompts
            .entry(submitted.session_prompt_id.to_string())
            .or_default()
            .started_at = Some(Instant::now());
    }

    fn handle_provider_response_updated(&mut self, update: &tau_proto::ProviderResponseUpdated) {
        let spid = update.session_prompt_id.as_str();
        self.update_editor_active_prompt(update);
        self.update_live_thinking_block(spid, update.thinking.as_deref());
        self.update_live_response_block(spid, &update.text);
    }

    fn update_editor_active_prompt(&mut self, update: &tau_proto::ProviderResponseUpdated) {
        if update.originator.is_user()
            && let Ok(mut context) = self.editor_context.lock()
        {
            context.active_prompt = if update.text.is_empty() {
                None
            } else {
                Some(update.text.clone())
            };
        }
    }

    fn update_live_thinking_block(&mut self, spid: &str, thinking: Option<&str>) {
        use tau_themes::names;

        let Some(thinking) = thinking else {
            return;
        };
        if thinking.is_empty() {
            return;
        }
        self.prompts
            .entry(spid.to_owned())
            .or_default()
            .thinking_text = Some(thinking.to_owned());
        if !self.show_thinking {
            return;
        }
        let block = streaming_block(&self.theme, names::AGENT_THINKING, thinking);
        let existing_tbid = self.prompts.get(spid).and_then(|s| s.thinking_block_id);
        if let Some(tbid) = existing_tbid {
            self.handle.set_block(tbid, block);
        } else {
            self.insert_live_thinking_block(spid, block);
        }
        self.handle.redraw();
    }

    fn insert_live_thinking_block(&mut self, spid: &str, block: tau_cli_term::StyledBlock) {
        // Insert the thinking block ABOVE the pending response block in
        // `above_active`. The response block was pushed first (in
        // SessionPromptCreated), so a plain push would land below it. Briefly
        // remove the response, push thinking, re-push response — net effect:
        // thinking is at the response's old position and the response moves
        // down by one.
        let tbid = self
            .handle
            .new_block(format!("agent-thinking-live:{spid}"), block);
        let response_bid = self.prompts.get(spid).and_then(|s| s.response_block_id);
        if let Some(response_bid) = response_bid {
            self.handle.remove_above_active(response_bid);
            self.handle.push_above_active(tbid);
            self.handle.push_above_active(response_bid);
        } else {
            self.handle.push_above_active(tbid);
        }
        self.prompts
            .entry(spid.to_owned())
            .or_default()
            .thinking_block_id = Some(tbid);
    }

    fn update_live_response_block(&mut self, spid: &str, text: &str) {
        use tau_themes::names;

        if let Some(bid) = self.prompts.get(spid).and_then(|s| s.response_block_id) {
            let block = streaming_block(&self.theme, names::AGENT_RESPONSE, text.to_owned());
            self.handle.set_block(bid, block);
            self.handle.redraw();
        }
    }

    fn handle_provider_response_finished(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) {
        let (prompt_state, turn_latency) = self.take_finished_prompt_state(finished);
        self.finalize_finished_thinking_block(
            prompt_state.thinking_block_id,
            prompt_state.thinking_text,
        );
        self.finalize_finished_response_block(prompt_state.response_block_id);

        let full_assistant_text = assistant_text_from_output_items(&finished.output_items);
        self.record_finished_assistant_context(finished, full_assistant_text.as_deref());
        self.record_finished_turn_stats(finished, turn_latency);
        self.render_user_provider_response_items(finished);
        self.render_model_status();
    }

    fn take_finished_prompt_state(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) -> (PromptState, Option<Duration>) {
        let spid = finished.session_prompt_id.as_str();
        // Drain the whole per-prompt state in one shot — every field tracked
        // through the stream is consumed here.
        let prompt_state = self.prompts.remove(spid).unwrap_or_default();
        let turn_latency = prompt_state
            .started_at
            .map(|started_at| started_at.elapsed());
        if let Some(latency) = turn_latency {
            self.cumulative_agent_latency += latency;
        }
        (prompt_state, turn_latency)
    }

    fn finalize_finished_thinking_block(
        &mut self,
        thinking_block_id: Option<tau_cli_term::BlockId>,
        thinking: Option<String>,
    ) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        // Finalize the thinking block above the response. The item-model finish
        // event no longer carries a separate thinking string; use the latest
        // streamed snapshot if one was captured.
        if let Some(tbid) = thinking_block_id {
            self.handle.remove_block(tbid);
        }
        if self.show_thinking
            && let Some(thinking) = thinking.filter(|t| !t.is_empty())
        {
            let bid = self.handle.print_output(
                "agent-thinking",
                themed_block(&self.theme, names::AGENT_THINKING, thinking.clone()),
            );
            self.thinking_history.push(ThinkingBlockEntry {
                block_id: bid,
                text: thinking,
            });
        }
    }

    fn finalize_finished_response_block(
        &mut self,
        response_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if let Some(bid) = response_block_id {
            self.handle.remove_block(bid);
        }
    }

    fn record_finished_assistant_context(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        full_assistant_text: Option<&str>,
    ) {
        let Some(text) = full_assistant_text else {
            return;
        };
        if finished.originator.is_user()
            && let Ok(mut context) = self.editor_context.lock()
        {
            context.last_agent_response = Some(text.to_owned());
            context.active_prompt = None;
        }
    }

    fn record_finished_turn_stats(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
        turn_latency: Option<Duration>,
    ) {
        let Some(usage) = finished.usage.clone() else {
            return;
        };
        let previous_usage = self.turn_stats_history.last().map(|entry| &entry.usage);
        let block = if self.show_turn_stats {
            render_turn_stats_block(
                &self.theme,
                &usage,
                previous_usage,
                turn_latency,
                Some(self.cumulative_agent_latency),
            )
        } else {
            Self::empty_block()
        };
        let bid = self.handle.print_output("turn-stats", block);
        self.turn_stats_history.push(TurnStatsBlockEntry {
            block_id: bid,
            usage,
            turn_latency,
            total_latency: Some(self.cumulative_agent_latency),
        });
    }

    fn render_user_provider_response_items(
        &mut self,
        finished: &tau_proto::ProviderResponseFinished,
    ) {
        // The event has already been routed into the owning agent transcript.
        // Only the main agent's tool calls land in the UI as their own blocks.
        // Sub-agent activity is summarized live under the parent's `delegate`
        // block via `DelegateProgress` instead, so the user sees one line per
        // delegation rather than a flood of nested invocations.
        self.main_agent_turn_active = true;
        let tool_calls = tool_calls_from_output_items(&finished.output_items);
        self.main_tools_total += tool_calls.len() as u64;
        self.set_main_tools_visible(!tool_calls.is_empty());
        let summary_block_id = self.prepare_tool_summary_for_finished_calls(&tool_calls);
        for item in &finished.output_items {
            self.render_finished_context_item(item, summary_block_id);
        }
        if !finished.output_items.is_empty() {
            self.handle.redraw();
        }
    }

    fn prepare_tool_summary_for_finished_calls(
        &mut self,
        tool_calls: &[ToolCallItem],
    ) -> Option<tau_cli_term::BlockId> {
        if tool_calls.is_empty() {
            self.finish_prompt_tool_summary();
            return None;
        }
        if matches!(
            self.show_tools,
            tau_config::settings::ShowTools::SummarizePrompt
        ) {
            return Some(self.create_or_update_prompt_tool_summary(tool_calls.len() as u64));
        }
        Some(self.create_turn_tool_summary(tool_calls.len() as u64))
    }

    fn create_or_update_prompt_tool_summary(&mut self, total_delta: u64) -> tau_cli_term::BlockId {
        if let Some(id) = self.prompt_tool_summary {
            if let Some(summary) = self.tool_summaries.get_mut(&id) {
                summary.total += total_delta;
            }
            if self.prompt_tool_summary_active {
                self.update_tool_summary_block(id);
                return id;
            }
            if let Some(summary) = self.tool_summaries.remove(&id) {
                return self.create_prompt_tool_summary(summary);
            }
        }
        let summary = ToolSummaryDisplay {
            total: total_delta,
            ..ToolSummaryDisplay::default()
        };
        self.create_prompt_tool_summary(summary)
    }

    fn create_prompt_tool_summary(&mut self, summary: ToolSummaryDisplay) -> tau_cli_term::BlockId {
        let block = self.render_summary_block(&summary);
        let id = self.handle.new_block("tool-summary:prompt", block);
        self.handle.push_above_active(id);
        self.tool_summaries.insert(id, summary);
        self.prompt_tool_summary = Some(id);
        self.prompt_tool_summary_active = true;
        id
    }

    fn finish_prompt_tool_summary(&mut self) {
        let Some(block_id) = self.prompt_tool_summary.take() else {
            self.prompt_tool_summary_active = false;
            return;
        };
        self.prompt_tool_summary_active = false;
        let Some(summary) = self.tool_summaries.remove(&block_id) else {
            return;
        };
        self.handle.remove_block(block_id);
        let new_block_id = self
            .handle
            .print_output("tool-summary", self.render_summary_block(&summary));
        self.tool_summaries.insert(new_block_id, summary);
    }

    fn create_turn_tool_summary(&mut self, total: u64) -> tau_cli_term::BlockId {
        let summary = ToolSummaryDisplay {
            total,
            ..ToolSummaryDisplay::default()
        };
        let block = self.render_summary_block(&summary);
        let id = self.handle.new_block("tool-summary:turn", block);
        self.handle.push_above_active(id);
        self.tool_summaries.insert(id, summary);
        id
    }

    fn render_finished_context_item(
        &mut self,
        item: &ContextItem,
        summary_block_id: Option<tau_cli_term::BlockId>,
    ) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        match item {
            ContextItem::Message(message) => {
                if let Some(text) = assistant_text_from_message_item(message) {
                    self.handle.print_output(
                        "agent-response",
                        themed_block(&self.theme, names::AGENT_RESPONSE, text),
                    );
                }
            }
            ContextItem::ToolCall(call) => {
                self.render_tool_call_placeholder(call, summary_block_id);
            }
            _ => {}
        }
    }

    fn render_tool_call_placeholder(
        &mut self,
        call: &ToolCallItem,
        summary_block_id: Option<tau_cli_term::BlockId>,
    ) {
        if self.tool_calls.contains_key(call.call_id.as_str()) {
            return;
        }
        let history_id = self.handle.new_block(
            format!("tool-call-history:{}:{}", call.name, call.call_id),
            Self::empty_block(),
        );
        self.handle.push_history(history_id);
        self.tool_calls.insert(
            call.call_id.to_string(),
            ToolCallState {
                history_block_id: Some(history_id),
                summary_block_id,
                is_main_delegate: call.name.as_str() == "delegate",
                ..ToolCallState::default()
            },
        );
    }

    fn handle_tool_started(&mut self, started: &tau_proto::ToolStarted, recorded_at: UnixMicros) {
        let call_id = started.call_id.to_string();
        if self
            .tool_calls
            .get(call_id.as_str())
            .is_some_and(|state| state.is_sub_agent || state.block_id.is_some())
        {
            return;
        }
        let call = ToolCallItem {
            call_id: started.call_id.clone(),
            name: started.tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
            arguments: started.arguments.clone(),
        };
        let display_payload = tool_display_from_call(&call);
        let mut display = format_tool_call(call.name.as_str(), Some(&display_payload));
        Self::upsert_tool_duration_suffix(&mut display, Duration::ZERO);
        let live_block = self.render_tool_history_block(&display);
        let live_id = self.handle.new_block(
            format!("tool-call-live:{}:{}", call.name, call.call_id),
            live_block,
        );
        self.handle.push_above_active(live_id);
        let state = self.tool_calls.entry(call_id).or_insert_with(|| {
            let history_id = self.handle.new_block(
                format!("tool-call-history:{}:{}", call.name, call.call_id),
                Self::empty_block(),
            );
            self.handle.push_history(history_id);
            ToolCallState {
                history_block_id: Some(history_id),
                is_main_delegate: call.name.as_str() == "delegate",
                ..ToolCallState::default()
            }
        });
        state.block_id = Some(live_id);
        state.live_display = Some(display);
        state.started_at = Some(Instant::now());
        state.recorded_started_at = Some(recorded_at);
        if let Some(timer) = &self.tool_timer {
            timer.tool_started(call.call_id.as_str());
        }
    }

    fn handle_tool_events(&mut self, event: &Event, recorded_at: UnixMicros) -> bool {
        match event {
            Event::ToolStarted(started) => {
                self.handle_tool_started(started, recorded_at);
                true
            }
            Event::ToolProgress(progress) => {
                self.handle_tool_progress(progress);
                true
            }
            Event::ToolDelegateProgress(progress) => {
                self.handle_tool_delegate_progress(progress);
                true
            }
            Event::ProviderToolResult(result)
                if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder =>
            {
                self.handle_tool_background_placeholder(result.call_id.as_str());
                true
            }
            Event::ProviderToolResult(result)
                if self.tool_calls.contains_key(result.call_id.as_str()) =>
            {
                self.handle_tool_result(result, recorded_at);
                true
            }
            Event::ProviderToolError(error) => {
                self.handle_tool_error(error, recorded_at);
                true
            }
            Event::ToolResult(result) => {
                self.handle_tool_result(result, recorded_at);
                true
            }
            Event::ToolError(error) => {
                self.handle_tool_error(error, recorded_at);
                true
            }
            Event::ToolBackgroundResult(result) => {
                self.handle_tool_background_result(result, recorded_at);
                true
            }
            Event::ToolBackgroundError(error) => {
                self.handle_tool_background_error(error, recorded_at);
                true
            }
            Event::ToolCancelled(cancelled) => {
                self.handle_tool_cancelled(cancelled, recorded_at);
                true
            }
            _ => false,
        }
    }

    fn handle_tool_progress(&mut self, progress: &tau_proto::ToolProgress) {
        use tau_cli_term::resolve::themed_block;
        use tau_themes::names;

        let state = self.tool_calls.get(progress.call_id.as_str());
        if state.is_some_and(|s| s.is_sub_agent) {
            return;
        }
        if state.is_none_or(|s| s.block_id.is_none()) {
            let text = tau_harness::format_tool_progress(progress);
            self.handle.print_output(
                "tool-progress",
                themed_block(&self.theme, names::SHELL_OUTPUT, text),
            );
        }
    }

    pub(crate) fn handle_tool_timer_tick(&mut self) {
        let mut changed = false;
        let mut updates = Vec::new();
        for (call_id, state) in &self.tool_calls {
            let (Some(block_id), Some(display)) = (state.block_id, state.live_display.as_ref())
            else {
                continue;
            };
            let Some(duration) = Self::live_tool_duration(state) else {
                continue;
            };
            let mut display = display.clone();
            Self::upsert_tool_duration_suffix(&mut display, duration);
            updates.push((call_id.clone(), block_id, display));
        }
        for (call_id, block_id, display) in updates {
            if let Some(state) = self.tool_calls.get_mut(&call_id) {
                state.live_display = Some(display.clone());
            }
            let block = self.render_tool_history_block(&display);
            self.handle.set_block(block_id, block);
            changed = true;
        }
        if changed {
            self.handle.redraw();
        }
    }

    fn live_tool_duration(state: &ToolCallState) -> Option<Duration> {
        if let Some(recorded_started_at) = state.recorded_started_at {
            let elapsed_micros = UnixMicros::now()
                .get()
                .checked_sub(recorded_started_at.get())?;
            return Some(Duration::from_micros(elapsed_micros));
        }
        state.started_at.map(|started_at| started_at.elapsed())
    }

    fn upsert_tool_duration_suffix(display: &mut ToolCallDisplay, duration: Duration) {
        use crate::tool_render::ToolStatus;

        display
            .suffixes
            .retain(|suffix| !matches!(suffix.status, ToolStatus::Time));

        let insert_at = display
            .suffixes
            .iter()
            .position(|suffix| {
                matches!(
                    suffix.status,
                    ToolStatus::Success
                        | ToolStatus::Warning
                        | ToolStatus::Error
                        | ToolStatus::Progress
                )
            })
            .unwrap_or(display.suffixes.len());

        let mut suffix = tool_duration_suffix(duration);
        if insert_at == 0
            && display
                .args
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            suffix.no_leading_space = true;
        }
        if let Some(next) = display.suffixes.get_mut(insert_at)
            && matches!(next.status, ToolStatus::Progress)
        {
            next.no_leading_space = false;
        }

        display.suffixes.insert(insert_at, suffix);
    }

    fn handle_tool_delegate_progress(&mut self, progress: &tau_proto::DelegateProgress) {
        let call_id = progress.call_id.as_str();
        let (bid, display) = {
            // Snapshot the latest counters and ctx info regardless of whether the
            // block is still live; the `ToolResult` handler reuses them on the
            // completion line.
            let state = self.tool_calls.entry(call_id.to_owned()).or_default();
            state.delegate_last_progress = Some(progress.clone());
            let Some(bid) = state.block_id else {
                // Block already torn down (delegate finished or never rendered) —
                // nothing to update.
                return;
            };
            let mut display = match &progress.display {
                Some(descriptor) => render_delegate_display(
                    descriptor,
                    progress.role.as_deref(),
                    progress.execution_mode,
                ),
                None => render_delegate_display(
                    &synthesize_fallback_display("delegate", None),
                    progress.role.as_deref(),
                    progress.execution_mode,
                ),
            };
            if let Some(duration) = Self::live_tool_duration(state) {
                Self::upsert_tool_duration_suffix(&mut display, duration);
            }
            state.live_display = Some(display.clone());
            (bid, display)
        };
        let block = self.render_tool_history_block(&display);
        self.handle.set_block(bid, block);
        if self.model_status_block.is_some() {
            self.render_model_status();
        } else {
            self.handle.redraw();
        }
    }

    fn take_finished_tool_call(
        &mut self,
        call_id: &str,
        originator_is_user: bool,
    ) -> Option<(ToolCallState, bool)> {
        let prior = self.tool_calls.remove(call_id);
        let known_main_tool = prior
            .as_ref()
            .is_some_and(|prior| !prior.is_sub_agent && originator_is_user);
        let prior = prior.unwrap_or_default();
        if prior.is_sub_agent {
            return None;
        }
        if let Some(block_id) = prior.block_id {
            if let Some(timer) = &self.tool_timer {
                timer.tool_finished(call_id);
            }
            self.handle.remove_block(block_id);
        }
        if known_main_tool {
            self.main_backgrounded_tools.remove(call_id);
            self.record_main_tool_completed();
            if self.main_agent_turn_active || !self.main_backgrounded_tools.is_empty() {
                self.main_tools_visible = true;
            }
        }
        Some((prior, known_main_tool))
    }

    fn handle_tool_result(&mut self, result: &tau_proto::ToolResult, recorded_at: UnixMicros) {
        let call_id = result.call_id.as_str();
        if result.kind == tau_proto::ToolResultKind::BackgroundPlaceholder {
            self.handle_tool_background_placeholder(call_id);
            return;
        }
        // Sub-agent tool activity stays out of the user's transcript — its
        // progress is rolled up under the parent's `delegate` block by
        // `DelegateProgress`.
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(call_id, result.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_result_display(result, prior.delegate_last_progress.as_ref());
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        let diff = Self::tool_result_diff(result);
        self.record_tool_summary_result(
            prior.summary_block_id,
            result.display.as_ref(),
            diff.as_ref(),
            false,
        );
        self.record_tool_result_block(prior.history_block_id, display, diff);
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn handle_tool_background_placeholder(&mut self, call_id: &str) {
        let Some(state) = self.tool_calls.get(call_id) else {
            return;
        };
        if state.is_sub_agent {
            return;
        }
        self.main_backgrounded_tools.insert(call_id.to_owned());
        self.main_tools_visible = true;
        self.render_model_status();
    }

    fn handle_tool_background_result(
        &mut self,
        result: &tau_proto::ToolBackgroundResult,
        recorded_at: UnixMicros,
    ) {
        let result = tau_proto::ToolResult {
            call_id: result.call_id.clone(),
            tool_name: result.tool_name.clone(),
            tool_type: result.tool_type,
            result: result.result.clone(),
            kind: tau_proto::ToolResultKind::Final,
            display: result.display.clone(),
            originator: result.originator.clone(),
        };
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(result.call_id.as_str(), result.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_result_display(&result, prior.delegate_last_progress.as_ref());
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        let diff = Self::tool_result_diff(&result);
        self.record_tool_summary_result(
            prior.summary_block_id,
            result.display.as_ref(),
            diff.as_ref(),
            false,
        );
        self.record_tool_result_block(prior.history_block_id, display, diff);
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn tool_result_display(
        result: &tau_proto::ToolResult,
        last_progress: Option<&tau_proto::DelegateProgress>,
    ) -> ToolCallDisplay {
        if result.tool_name.as_str() == "delegate" {
            let role = last_progress.and_then(|p| p.role.as_deref());
            let execution_mode = last_progress.and_then(|p| p.execution_mode);
            let descriptor = build_delegate_completion_display(
                last_progress.and_then(|p| p.display.as_ref()),
                &result.result,
                None,
            );
            render_delegate_display(&descriptor, role, execution_mode)
        } else if let Some(descriptor) = &result.display {
            render_tool_display(&result.tool_name, descriptor)
        } else {
            render_tool_display(
                &result.tool_name,
                &synthesize_fallback_display(&result.tool_name, None),
            )
        }
    }

    fn finished_tool_duration(prior: &ToolCallState, finished_at: UnixMicros) -> Option<Duration> {
        let started_at = prior.recorded_started_at?;
        let elapsed_micros = finished_at.get().checked_sub(started_at.get())?;
        Some(Duration::from_micros(elapsed_micros))
    }

    fn tool_result_diff(result: &tau_proto::ToolResult) -> Option<tau_proto::DiffSummary> {
        result
            .display
            .as_ref()
            .and_then(|d| match &d.payload {
                Some(tau_proto::ToolDisplayPayload::Diff(s)) => Some(s.clone()),
                _ => None,
            })
            .or_else(|| extract_diff(&result.result))
    }

    fn record_tool_result_block(
        &mut self,
        existing_block_id: Option<tau_cli_term::BlockId>,
        display: ToolCallDisplay,
        diff: Option<tau_proto::DiffSummary>,
    ) {
        if let Some(diff) = diff {
            let block = self.render_diff_history_block(&display, &diff);
            let bid =
                self.update_existing_or_print_tool_block(existing_block_id, "tool-diff", block);
            self.diff_blocks.push(DiffBlockEntry {
                block_id: bid,
                display,
                diff,
            });
        } else {
            let block = self.render_tool_history_block(&display);
            let bid =
                self.update_existing_or_print_tool_block(existing_block_id, "tool-result", block);
            self.tool_history.push(ToolBlockEntry {
                block_id: bid,
                display,
            });
        }
    }

    fn handle_tool_error(&mut self, error: &tau_proto::ToolError, recorded_at: UnixMicros) {
        let call_id = error.call_id.as_str();
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(call_id, error.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_error_display(error, prior.delegate_last_progress.as_ref());
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        self.record_tool_summary_result(prior.summary_block_id, error.display.as_ref(), None, true);
        self.record_plain_finished_tool_block(prior.history_block_id, display, "tool-error");
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn handle_tool_background_error(
        &mut self,
        error: &tau_proto::ToolBackgroundError,
        recorded_at: UnixMicros,
    ) {
        let error = tau_proto::ToolError {
            call_id: error.call_id.clone(),
            tool_name: error.tool_name.clone(),
            tool_type: error.tool_type,
            message: error.message.clone(),
            details: error.details.clone(),
            display: error.display.clone(),
            originator: error.originator.clone(),
        };
        let Some((prior, known_main_tool)) =
            self.take_finished_tool_call(error.call_id.as_str(), error.originator.is_user())
        else {
            return;
        };
        let mut display = Self::tool_error_display(&error, prior.delegate_last_progress.as_ref());
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        self.record_tool_summary_result(prior.summary_block_id, error.display.as_ref(), None, true);
        self.record_plain_finished_tool_block(prior.history_block_id, display, "tool-error");
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn tool_error_display(
        error: &tau_proto::ToolError,
        last_progress: Option<&tau_proto::DelegateProgress>,
    ) -> ToolCallDisplay {
        let cbor = error.details.as_ref();
        if error.tool_name.as_str() == "delegate" {
            let role = last_progress.and_then(|p| p.role.as_deref());
            let execution_mode = last_progress.and_then(|p| p.execution_mode);
            let descriptor = build_delegate_completion_display(
                last_progress.and_then(|p| p.display.as_ref()),
                cbor.unwrap_or(&CborValue::Null),
                Some(&error.message),
            );
            render_delegate_display(&descriptor, role, execution_mode)
        } else if let Some(descriptor) = &error.display {
            render_tool_display(&error.tool_name, descriptor)
        } else {
            render_tool_display(
                &error.tool_name,
                &synthesize_fallback_display(&error.tool_name, Some(&error.message)),
            )
        }
    }

    fn handle_tool_cancelled(
        &mut self,
        cancelled: &tau_proto::ToolCancelled,
        recorded_at: UnixMicros,
    ) {
        let call_id = cancelled.call_id.as_str();
        let Some((prior, known_main_tool)) = self.take_finished_tool_call(call_id, true) else {
            return;
        };
        let mut display = render_tool_display(
            &cancelled.tool_name,
            &synthesize_fallback_display(&cancelled.tool_name, Some("cancelled")),
        );
        if let Some(duration) = Self::finished_tool_duration(&prior, recorded_at) {
            Self::upsert_tool_duration_suffix(&mut display, duration);
        }
        self.record_tool_summary_result(prior.summary_block_id, None, None, true);
        self.record_plain_finished_tool_block(prior.history_block_id, display, "tool-cancelled");
        self.render_model_status_after_tool_completion(known_main_tool);
    }

    fn record_plain_finished_tool_block(
        &mut self,
        existing_block_id: Option<tau_cli_term::BlockId>,
        display: ToolCallDisplay,
        label: &'static str,
    ) {
        let block = self.render_tool_history_block(&display);
        let bid = self.update_existing_or_print_tool_block(existing_block_id, label, block);
        self.tool_history.push(ToolBlockEntry {
            block_id: bid,
            display,
        });
    }

    fn update_existing_or_print_tool_block(
        &mut self,
        existing_block_id: Option<tau_cli_term::BlockId>,
        label: &'static str,
        block: tau_cli_term::StyledBlock,
    ) -> tau_cli_term::BlockId {
        if let Some(bid) = existing_block_id {
            self.handle.set_block(bid, block);
            self.handle.redraw();
            bid
        } else {
            self.handle.print_output(label, block)
        }
    }

    fn render_model_status_after_tool_completion(&mut self, known_main_tool: bool) {
        if known_main_tool && self.main_agent_turn_active {
            self.render_model_status();
        }
    }

    fn handle_shell_events(&mut self, event: &Event) -> bool {
        match event {
            Event::UiShellCommand(cmd) => {
                self.handle_ui_shell_command(cmd);
                true
            }
            Event::ShellCommandProgress(progress) => {
                self.handle_shell_command_progress(progress);
                true
            }
            Event::ShellCommandFinished(finished) => {
                self.handle_shell_command_finished(finished);
                true
            }
            _ => false,
        }
    }

    fn shell_running_label(include_in_context: bool) -> String {
        if include_in_context {
            "running".to_owned()
        } else {
            "running [no context]".to_owned()
        }
    }

    fn handle_ui_shell_command(&mut self, cmd: &tau_proto::UiShellCommand) {
        // Create a running block now; the harness will echo progress and a
        // finished event back to us via the bus. Both bangs render the same;
        // the context bit just labels the suffix.
        let label = Self::shell_running_label(cmd.include_in_context);
        let block = render_shell_block(&self.theme, &cmd.command, "", Some(&label));
        let block_id = self
            .handle
            .new_block(format!("shell-command:{}", cmd.command_id), block);
        self.handle.push_above_active(block_id);
        self.handle.redraw();
        self.shell_blocks.insert(
            cmd.command_id.to_string(),
            ShellBlockState {
                block_id,
                command: cmd.command.clone(),
                include_in_context: cmd.include_in_context,
                output: String::new(),
            },
        );
    }

    fn handle_shell_command_progress(&mut self, progress: &tau_proto::ShellCommandProgress) {
        if let Some(state) = self.shell_blocks.get_mut(progress.command_id.as_str()) {
            state.output.push_str(&progress.chunk);
            let label = Self::shell_running_label(state.include_in_context);
            let block =
                render_shell_block(&self.theme, &state.command, &state.output, Some(&label));
            self.handle.set_block(state.block_id, block);
            self.handle.redraw();
        }
    }

    fn handle_shell_command_finished(&mut self, finished: &tau_proto::ShellCommandFinished) {
        let Some(state) = self.shell_blocks.remove(finished.command_id.as_str()) else {
            return;
        };
        // Use the final, post-truncation output from the extension rather than
        // our streaming buffer so the UI matches what the harness injected into
        // context.
        self.handle.remove_block(state.block_id);
        let suffix = Self::shell_finished_suffix(finished, state.include_in_context);
        let block = render_shell_block(
            &self.theme,
            &finished.command,
            &finished.output,
            Some(&suffix),
        );
        self.handle.print_output("shell-finished", block);
    }

    fn shell_finished_suffix(
        finished: &tau_proto::ShellCommandFinished,
        include_in_context: bool,
    ) -> String {
        let suffix = if finished.cancelled {
            "cancelled".to_owned()
        } else {
            match finished.exit_code {
                Some(0) => "[0]".to_owned(),
                Some(code) => format!("[{code}]"),
                None => "[?]".to_owned(),
            }
        };
        if include_in_context {
            suffix
        } else {
            format!("{suffix} [no context]")
        }
    }

    fn handle_action_events(&mut self, event: &Event) -> bool {
        match event {
            Event::ActionSchemaPublished(published) => {
                self.action_state.apply_schema_published(published);
                self.refresh_action_completions();
                true
            }
            Event::ActionResult(result) => {
                self.handle_action_result(result);
                true
            }
            Event::ActionError(error) => {
                self.handle_action_error(error);
                true
            }
            Event::ActionInvoke(_) => true,
            _ => false,
        }
    }

    fn refresh_action_completions(&self) {
        let (commands, arg_completers) = self.action_state.dynamic_completions();
        self.completion_data
            .set_dynamic_commands_and_arg_completers(commands, arg_completers);
    }

    fn handle_action_result(&mut self, result: &tau_proto::ActionResult) {
        use crate::tool_render::render_action_output_block;

        let text = match &result.output {
            tau_proto::ActionOutput::Text { text } => text.clone(),
            tau_proto::ActionOutput::EditorBuffer {
                title,
                text,
                editable,
            } => {
                let mut rendered = format!("{title}\n{text}");
                if *editable {
                    rendered.push_str("\n[editable buffer]");
                }
                rendered
            }
        };
        self.handle.print_output(
            "action-result",
            render_action_output_block(&self.theme, &text),
        );
    }

    fn handle_action_error(&mut self, error: &tau_proto::ActionError) {
        use crate::tool_render::render_action_error_block;

        self.handle.print_output(
            "action-error",
            render_action_error_block(&self.theme, &error.action_id, &error.message),
        );
    }

    fn handle_extension_events(&mut self, event: &Event) -> bool {
        match event {
            Event::ExtensionStarting(starting) => {
                self.handle_extension_starting(starting);
                true
            }
            Event::ExtensionReady(ready) => {
                self.handle_extension_ready(ready);
                true
            }
            Event::ExtensionExited(exited) => {
                self.action_state
                    .remove_extension(&exited.extension_name, exited.instance_id);
                self.refresh_action_completions();
                self.handle_extension_exited(exited);
                true
            }
            Event::ExtAgentsMdAvailable(agents) => {
                self.handle_agents_md_available(agents);
                true
            }
            Event::ExtensionContextReady(_) => {
                self.handle_extension_context_ready();
                true
            }
            _ => false,
        }
    }

    fn handle_extension_starting(&mut self, starting: &tau_proto::ExtensionStarting) {
        let block = extension_status_block(&self.theme, &starting.extension_name, "starting");
        let id = self.handle.new_block(
            format!("extension-starting:{}", starting.instance_id),
            block,
        );
        self.handle.push_above_active(id);
        self.handle.redraw();
        self.extension_blocks.insert(starting.instance_id, id);
    }

    fn handle_extension_ready(&mut self, ready: &tau_proto::ExtensionReady) {
        if let Some(bid) = self.extension_blocks.remove(&ready.instance_id) {
            self.handle.remove_block(bid);
        }
        self.ready_extensions
            .insert(ready.extension_name.to_string());
        self.handle.print_output(
            "extension-ready",
            extension_status_block(&self.theme, &ready.extension_name, "ready"),
        );
    }

    fn handle_extension_exited(&mut self, exited: &tau_proto::ExtensionExited) {
        if let Some(bid) = self.extension_blocks.remove(&exited.instance_id) {
            self.handle.remove_block(bid);
        }
        self.ready_extensions.remove(exited.extension_name.as_str());
        self.handle.print_output(
            "extension-exited",
            extension_status_block(&self.theme, &exited.extension_name, "exited"),
        );
    }

    fn handle_agents_md_available(&mut self, agents: &tau_proto::ExtAgentsMdAvailable) {
        self.handle.print_output(
            "agents-md",
            system_loaded_block(&self.theme, &agents.file_path, &agents.content),
        );
    }

    fn handle_extension_context_ready(&mut self) {
        self.handle.print_output(
            "extension-context-ready",
            system_status_block(&self.theme, "session context ", "ready"),
        );
    }

    fn handle_harness_status_events(&mut self, event: &Event) -> bool {
        match event {
            Event::HarnessInfo(info) => {
                self.handle
                    .print_output("harness-info", render_harness_info(&self.theme, info));
                true
            }
            Event::HarnessSessionDir(session_dir) => {
                self.handle_harness_session_dir(session_dir);
                true
            }
            Event::HarnessUiDir(ui_dir) => {
                self.handle
                    .print_output("ui-dir", ui_dir_block(&self.theme, &ui_dir.path));
                true
            }
            Event::HarnessModelsAvailable(_models) => true,
            _ => false,
        }
    }

    fn handle_harness_session_dir(&mut self, session_dir: &tau_proto::HarnessSessionDir) {
        self.handle.print_output(
            "session-dir",
            session_status_block(
                &self.theme,
                &session_dir.path,
                "/",
                session_dir.status.as_str(),
            ),
        );
    }

    fn handle_harness_role_events(&mut self, event: &Event) -> bool {
        match event {
            Event::HarnessRolesAvailable(roles) => {
                self.handle_harness_roles_available(roles);
                true
            }
            Event::HarnessRoleSelected(selected) => {
                self.handle_harness_role_selected(selected);
                true
            }
            Event::HarnessContextUsageChanged(changed) => {
                self.current_context_input_tokens = changed.input_tokens;
                self.current_context_percent = changed.percent_used;
                self.render_model_status();
                true
            }
            _ => false,
        }
    }

    fn handle_harness_roles_available(&mut self, roles: &tau_proto::HarnessRolesAvailable) {
        let model_items: Vec<tau_cli_term::CompletionItem> = roles
            .roles
            .iter()
            .map(|r| tau_cli_term::CompletionItem::new(&r.name, &r.description))
            .collect();
        let role_defaults: HashMap<String, RoleCompletionDetails> = roles
            .roles
            .iter()
            .map(|r| (r.name.clone(), RoleCompletionDetails::from_role_info(r)))
            .collect();
        let role_items = Self::role_completion_items(roles, &role_defaults);
        if let Ok(mut available) = self.roles_available.lock() {
            *available = roles.roles.iter().map(|r| r.name.clone()).collect();
        }
        if let Ok(mut available) = self.role_groups_available.lock() {
            *available = roles.groups.clone();
        }
        self.role_defaults = role_defaults;
        if self.current_role.is_some() && self.model_status_block.is_some() {
            self.render_model_status();
        }
        self.completion_data
            .set_arg_completions(tau_cli_term::CommandName::new("/model"), model_items);
        let completer: tau_cli_term::ArgCompleter =
            std::sync::Arc::new(move |args| role_command_completions(&role_items, args));
        self.completion_data
            .set_arg_completer(tau_cli_term::CommandName::new("/role"), completer);
    }

    fn role_completion_items(
        roles: &tau_proto::HarnessRolesAvailable,
        role_defaults: &HashMap<String, RoleCompletionDetails>,
    ) -> Vec<(tau_cli_term::CompletionItem, RoleCompletionDetails)> {
        roles
            .roles
            .iter()
            .filter_map(|role| {
                let details = role_defaults.get(&role.name)?.clone();
                Some((
                    tau_cli_term::CompletionItem::new(&role.name, details.short_description()),
                    details,
                ))
            })
            .collect()
    }

    fn handle_harness_role_selected(&mut self, selected: &tau_proto::HarnessRoleSelected) {
        self.current_model = selected.model.clone();
        self.current_role = Some(selected.role.clone());
        self.baseline_params = selected.baseline_params;
        self.model_params = selected.model_params;
        self.effort_state.store(
            selected.model_params.effort.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.verbosity_state.store(
            selected.model_params.verbosity.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.thinking_summary_state.store(
            selected.model_params.thinking_summary.as_u8(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.fast_service_tier_state.store(
            matches!(
                selected.model_params.service_tier,
                Some(tau_proto::ServiceTier::Fast)
            ),
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Ok(mut role) = self.current_role_state.lock() {
            *role = Some(selected.role.clone());
        }
        if let (Ok(groups), Ok(mut memory)) = (
            self.role_groups_available.lock(),
            self.role_group_memory.lock(),
        ) && let Some(group) = groups
            .iter()
            .find(|group| group.roles.iter().any(|role| role == &selected.role))
        {
            memory.insert(group.name.clone(), selected.role.clone());
        }
        let prompt = crate::theme::active_prompt_marker(
            &self.theme,
            &self.prompt_symbol,
            Some(&selected.role),
        );
        self.handle.set_left_prompt(prompt);
        self.handle
            .set_input_placeholder(crate::theme::prompt_input_placeholder(
                &self.theme,
                Some(&selected.role),
            ));
        self.handle.redraw();
        self.current_context_window = selected.context_window;
        self.render_model_status();
    }

    fn handle_harness_available_events(&mut self, event: &Event) -> bool {
        match event {
            Event::HarnessEffortsAvailable(avail) => {
                self.handle_harness_efforts_available(avail);
                true
            }
            Event::HarnessVerbositiesAvailable(avail) => {
                self.handle_harness_verbosities_available(avail);
                true
            }
            Event::HarnessThinkingSummariesAvailable(avail) => {
                self.handle_harness_thinking_summaries_available(avail);
                true
            }
            _ => false,
        }
    }

    fn handle_harness_efforts_available(&mut self, avail: &tau_proto::HarnessEffortsAvailable) {
        let _ = avail;
    }

    fn handle_harness_verbosities_available(
        &mut self,
        avail: &tau_proto::HarnessVerbositiesAvailable,
    ) {
        let _ = avail;
    }

    fn handle_harness_thinking_summaries_available(
        &mut self,
        avail: &tau_proto::HarnessThinkingSummariesAvailable,
    ) {
        let _ = avail;
    }

    fn handle_terminal_events(&mut self, event: &Event) -> bool {
        match event {
            Event::Osc1337SetUserVar(req) => {
                let in_tmux = std::env::var_os("TMUX").is_some();
                let seq = build_osc1337_set_user_var(&req.name, &req.value, in_tmux);
                self.handle.print_terminal_escape(seq);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests;
