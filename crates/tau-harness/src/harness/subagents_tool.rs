//! Harness-owned `delegate` and `wait` tools.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tau_proto::{
    BackgroundSupport, CborValue, Event, SessionContextKey, SessionContextValue,
    ToolBackgroundError, ToolBackgroundResult, ToolCallId, ToolDisplay, ToolDisplayStats,
    ToolError, ToolExecutionMode, ToolName, ToolRequest, ToolResult, ToolResultKind, ToolSpec,
    ToolType,
};

use crate::conversation::ConversationId;
use crate::error::HarnessError;
use crate::harness::{AgentToolCall, HARNESS_CONNECTION_ID, Harness, PendingTool};

/// Model-visible name of the harness-owned delegate tool.
pub(crate) const DELEGATE_TOOL_NAME: &str = "delegate";
/// Model-visible name of the harness-owned wait tool.
pub(crate) const WAIT_TOOL_NAME: &str = "wait";

const DELEGATE_PREFIX: &str = include_str!("prompts/delegate_prefix.md");
const SLOW_DELEGATE_EXEC_TIME_THRESHOLD_SECS: u64 = 5;

#[derive(Clone, Debug)]
pub(crate) struct PendingHarnessDelegate {
    /// Original tool call id that should receive the side-agent answer.
    pub(crate) call_id: ToolCallId,
    /// Visible tool name for the original call.
    pub(crate) tool_name: ToolName,
    /// Wall-clock start time used for slow-call duration metadata.
    pub(crate) started_at: Instant,
}

#[derive(Default)]
pub(crate) struct SubagentToolState {
    /// Outstanding harness-owned delegations indexed by query id.
    pub(crate) pending_delegates: HashMap<String, PendingHarnessDelegate>,
    /// Monotonic id used to create delegate query ids.
    pub(crate) next_delegate_query_id: u64,
    /// State used by the wait tool to track background completions.
    wait_tracker: WaitTracker,
}

impl Harness {
    /// Register harness-owned tools.
    pub(crate) fn register_harness_tools(&mut self) {
        self.register_skill_tool();
        let _ = self
            .registry
            .register(HARNESS_CONNECTION_ID, delegate_tool_spec());
        let _ = self
            .registry
            .register(HARNESS_CONNECTION_ID, wait_tool_spec());
    }

    pub(crate) fn publish_delegate_roles_context(&mut self) {
        let mut roles: Vec<_> = crate::model::role_infos(
            &self.provider_model_info,
            &self.available_roles,
            &self.available_models,
        )
        .into_iter()
        .filter(|info| {
            crate::model::model_for_role(
                &self.provider_model_info,
                &self.available_roles,
                &info.name,
            )
            .is_some()
        })
        .map(|info| {
            let description = info
                .role_description
                .as_deref()
                .filter(|description| !description.is_empty())
                .unwrap_or(&info.description);
            serde_json::json!({
                "name": info.name,
                "description": description,
            })
        })
        .collect();
        roles.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        self.session_context.publish(
            self.current_session_id.clone(),
            SessionContextKey::new("delegate_roles"),
            tau_proto::ConnectionId::from(HARNESS_CONNECTION_ID),
            "harness".to_owned(),
            SessionContextValue(serde_json::Value::Array(roles)),
        );
    }

    pub(crate) fn record_wait_tool_request(&mut self, call_id: &ToolCallId) {
        if let Some(tool) = self.pending_tools.get(call_id) {
            self.subagents
                .wait_tracker
                .record_tool_invoke(call_id.clone(), tool.name.clone());
        }
    }

    pub(crate) fn record_wait_tool_result(&mut self, result: ToolResult) {
        let replies = self.subagents.wait_tracker.record_tool_result(result);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_tool_error(&mut self, error: ToolError) {
        let replies = self.subagents.wait_tracker.record_tool_error(error);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_background_result(&mut self, result: ToolBackgroundResult) {
        let replies = self.subagents.wait_tracker.record_background_result(result);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_background_error(&mut self, error: ToolBackgroundError) {
        let replies = self.subagents.wait_tracker.record_background_error(error);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn interrupt_active_waits(&mut self) {
        let replies = self.subagents.wait_tracker.interrupt_active_waits();
        self.publish_wait_replies(replies);
    }

    /// Handle the harness-owned `delegate` tool call inline.
    pub(crate) fn handle_delegate_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        self.track_harness_owned_tool_request(cid, call, &visible_tool_name);
        self.record_wait_tool_request(&call_id);

        let parsed = match parse_delegate_args(&call.arguments) {
            Ok(parsed) => parsed,
            Err(message) => {
                self.finish_harness_owned_tool_with_error(
                    cid,
                    call_id,
                    visible_tool_name,
                    call.tool_type,
                    message,
                    Some(call.arguments.clone()),
                );
                return Ok(());
            }
        };

        let query_id = format!("delegate-{}", self.subagents.next_delegate_query_id);
        self.subagents.next_delegate_query_id += 1;
        self.subagents.pending_delegates.insert(
            query_id.clone(),
            PendingHarnessDelegate {
                call_id: call_id.clone(),
                tool_name: visible_tool_name,
                started_at: Instant::now(),
            },
        );
        if self.tool_turn.mark_backgrounded(&call_id) {
            self.publish_synthetic_background_result(&call_id);
        }
        self.handle_ext_agent_query(
            HARNESS_CONNECTION_ID,
            tau_proto::ExtAgentQuery {
                query_id,
                instruction: format!("{DELEGATE_PREFIX}{}", parsed.prompt),
                role: parsed.role,
                execution_mode: parsed.execution_mode,
                input_stats: ToolDisplayStats::for_text(&parsed.prompt),
                tool_call_id: Some(call_id),
                task_name: Some(parsed.task_name),
            },
        )
    }

    /// Handle the harness-owned `wait` tool call inline.
    pub(crate) fn handle_wait_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        self.track_harness_owned_tool_request(cid, call, &visible_tool_name);
        let start = self.subagents.wait_tracker.handle_wait_invoke(
            call_id,
            visible_tool_name,
            &call.arguments,
        );
        if let Some(target) = start.suppress_call_id {
            self.suppress_background_completion_prompt(target);
        }
        self.publish_wait_replies(start.reply.into_iter().collect());
        Ok(())
    }

    pub(crate) fn complete_harness_delegate(
        &mut self,
        _cid: &ConversationId,
        query_id: &str,
        text: String,
        error: Option<String>,
    ) {
        let Some(pending) = self.subagents.pending_delegates.remove(query_id) else {
            return;
        };
        let duration_seconds = delegate_duration_seconds(pending.started_at.elapsed());
        let call_id = pending.call_id.clone();
        let owner_cid = self
            .tool_conversations
            .get(&call_id)
            .cloned()
            .unwrap_or_else(|| self.default_conversation_id.clone());
        if let Some(message) = error {
            let event = ToolError {
                call_id: call_id.clone(),
                tool_name: pending.tool_name,
                tool_type: ToolType::Function,
                message,
                details: delegate_error_details(duration_seconds),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            };
            if self.tool_turn.is_backgrounded(&call_id) {
                self.handle_background_tool_error(HARNESS_CONNECTION_ID, event);
            } else {
                self.publish_terminal_tool_error(Some(&owner_cid), None, event);
                self.on_tool_call_complete(call_id.as_str());
                self.clear_tool_call_tracking(call_id.as_str());
            }
        } else {
            let event = ToolResult {
                call_id: call_id.clone(),
                tool_name: pending.tool_name,
                tool_type: ToolType::Function,
                result: delegate_result_value(text, duration_seconds),
                kind: ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            };
            if self.tool_turn.is_backgrounded(&call_id) {
                self.handle_background_tool_result(HARNESS_CONNECTION_ID, event);
            } else {
                self.publish_for_conversation(&owner_cid, Event::ToolResult(event.clone()));
                self.record_wait_tool_result(event);
                self.on_tool_call_complete(call_id.as_str());
                self.clear_tool_call_tracking(call_id.as_str());
            }
        }
    }

    fn track_harness_owned_tool_request(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: &ToolName,
    ) {
        let call_id: ToolCallId = call.id.clone();
        self.tool_conversations.insert(call_id.clone(), cid.clone());
        self.pending_tools.insert(
            call_id.clone(),
            PendingTool {
                name: visible_tool_name.clone(),
                tool_type: call.tool_type,
            },
        );
        self.bump_tools_started_for(cid);
        self.publish_for_conversation(
            cid,
            Event::ToolRequest(ToolRequest {
                call_id,
                tool_name: visible_tool_name.clone(),
                tool_type: call.tool_type,
                arguments: call.arguments.clone(),
                originator: tau_proto::PromptOriginator::User,
            }),
        );
    }

    fn finish_harness_owned_tool_with_error(
        &mut self,
        cid: &ConversationId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        message: String,
        details: Option<CborValue>,
    ) {
        let error = ToolError {
            call_id: call_id.clone(),
            tool_name,
            tool_type,
            message,
            details,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        };
        self.publish_terminal_tool_error(Some(cid), None, error);
        self.on_tool_call_complete(call_id.as_str());
        self.clear_tool_call_tracking(call_id.as_str());
    }

    fn publish_wait_replies(&mut self, replies: Vec<WaitReply>) {
        for reply in replies {
            if let Some(call_id) = reply.unsuppress_call_id.clone() {
                self.unsuppress_background_completion_prompt(call_id);
            }
            if let Some(call_id) = reply.suppress_call_id.clone() {
                self.suppress_background_completion_prompt(call_id);
            }
            let wait_call_id = reply.wait_call_id.clone();
            let Some(cid) = self.tool_conversations.get(&wait_call_id).cloned() else {
                continue;
            };
            let event = match reply.kind {
                WaitReplyKind::Result { result, display } => Event::ToolResult(ToolResult {
                    call_id: reply.wait_call_id,
                    tool_name: reply.wait_tool_name,
                    tool_type: ToolType::Function,
                    result,
                    kind: ToolResultKind::Final,
                    display,
                    originator: tau_proto::PromptOriginator::User,
                }),
                WaitReplyKind::Error {
                    message,
                    details,
                    display,
                } => Event::ToolError(ToolError {
                    call_id: reply.wait_call_id,
                    tool_name: reply.wait_tool_name,
                    tool_type: ToolType::Function,
                    message,
                    details,
                    display,
                    originator: tau_proto::PromptOriginator::User,
                }),
            };
            self.publish_for_conversation(&cid, event);
            self.on_tool_call_complete(wait_call_id.as_str());
            self.clear_tool_call_tracking(wait_call_id.as_str());
        }
    }
}

fn delegate_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(DELEGATE_TOOL_NAME),
        model_visible_name: None,
        description: Some("Delegate a self-contained sub-task to a fresh sub-agent that runs with its own context and tools, and returns only its final text answer. Use it for: open-ended exploration where step count is unpredictable; large search/read sweeps whose intermediate output would otherwise clutter this conversation; parallel work — multiple delegations with `execution_mode: \"shared\"` can overlap globally. Use `execution_mode: \"exclusive\"` when the sub-agent needs to run alone: it waits for all other sub-agent delegations and blocks later independent ones until it finishes. Skip it when the target is already known (use direct tools like `read`/`grep`/`shell` instead) or when the task requires synthesis you should do yourself — don't push 'based on findings, fix the bug' onto a sub-agent; investigate first, then delegate the concrete change. The sub-agent starts with a *clean* conversation: it sees ONLY your `prompt`, plus its tools and system prompt. It cannot see this conversation's prior turns, your reasoning, files you've read, or earlier tool results — and that isolation applies at every nesting depth, so a sub-agent's own delegations are equally fresh. You must therefore brief the sub-agent fully: state the goal, hand it every fact it needs (absolute file paths, exact symbols, code snippets, prior findings, constraints, format of the answer you want), and frame the sub-task as if writing to a teammate who just walked into the room. Terse command-style prompts produce shallow, generic work; missing context produces wrong answers.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "task_name": { "type": "string", "description": "Short human-readable label for the sub-task (a few words, lowercase). Surfaced live to the user as `delegate [task_name]` while the sub-agent runs." },
                "prompt": { "type": "string", "description": "Self-contained task for the sub-agent. The sub-agent's conversation starts fresh — it has NO access to this conversation's history, your earlier tool results, or files you've read. State everything it needs: the goal, the relevant facts (absolute file paths, exact symbols, snippets you've already extracted), any constraints, what counts as 'done', and the format of the answer you want back. Treat it like briefing a teammate who just walked into the room. Terse command-style prompts produce shallow work; missing context produces wrong answers." },
                "execution_mode": { "type": "string", "enum": ["shared", "exclusive"], "description": "Use `shared` when the sub-task can safely overlap globally with other shared sub-agent delegations. Use `exclusive` when it must run alone: it waits for all other sub-agent delegations and blocks later independent ones. Default: `shared`." },
                "role": { "type": "string", "description": "Optional sub-agent role to use. When omitted, Tau defaults delegate calls to `engineer` if that role is available and enabled." }
            },
            "required": ["task_name", "prompt"]
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Instant),
    }
}

fn wait_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(WAIT_TOOL_NAME),
        model_visible_name: None,
        description: Some("Wait for a tool call by `tool_call_id` and return its completed result. Tau will notify you via marked internal messages about tool calls running in the background, and when they complete. Prefer calling this after Tau reports completion for efficiency reasons; it also works for calls that are still running.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": { "tool_call_id": { "type": "string", "description": "The tool call id to wait for." } },
            "required": ["tool_call_id"]
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Never),
    }
}

struct DelegateArgs {
    task_name: String,
    prompt: String,
    execution_mode: ToolExecutionMode,
    role: Option<String>,
}

fn parse_delegate_args(arguments: &CborValue) -> Result<DelegateArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut prompt = None;
    let mut task_name = None;
    let mut execution_mode = None;
    let mut role = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "prompt" => match v {
                CborValue::Text(text) => prompt = Some(text.clone()),
                _ => return Err("`prompt` must be a string".to_owned()),
            },
            "task_name" => match v {
                CborValue::Text(text) => task_name = Some(text.clone()),
                _ => return Err("`task_name` must be a string".to_owned()),
            },
            "role" => match v {
                CborValue::Text(text) => role = Some(text.clone()),
                _ => return Err("`role` must be a string".to_owned()),
            },
            "execution_mode" => match v {
                CborValue::Text(text) if text == "shared" => {
                    execution_mode = Some(ToolExecutionMode::Shared)
                }
                CborValue::Text(text) if text == "exclusive" => {
                    execution_mode = Some(ToolExecutionMode::Exclusive)
                }
                CborValue::Text(_) => {
                    return Err("`execution_mode` must be `shared` or `exclusive`".to_owned());
                }
                _ => return Err("`execution_mode` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let prompt = prompt.ok_or_else(|| "missing string argument: prompt".to_owned())?;
    if prompt.trim().is_empty() {
        return Err("`prompt` must not be empty".to_owned());
    }
    let task_name = task_name.ok_or_else(|| "missing string argument: task_name".to_owned())?;
    if task_name.trim().is_empty() {
        return Err("`task_name` must not be empty".to_owned());
    }
    Ok(DelegateArgs {
        task_name,
        prompt,
        execution_mode: execution_mode.unwrap_or(ToolExecutionMode::Shared),
        role: role.filter(|role| !role.trim().is_empty()),
    })
}

fn delegate_duration_seconds(elapsed: Duration) -> Option<u64> {
    if Duration::from_secs(SLOW_DELEGATE_EXEC_TIME_THRESHOLD_SECS) < elapsed {
        Some(elapsed.as_secs_f64().ceil() as u64)
    } else {
        None
    }
}

fn delegate_result_value(text: String, duration_seconds: Option<u64>) -> CborValue {
    let Some(duration_seconds) = duration_seconds else {
        return CborValue::Text(text);
    };
    CborValue::Map(delegate_detail_entries(Some(text), duration_seconds))
}

fn delegate_error_details(duration_seconds: Option<u64>) -> Option<CborValue> {
    duration_seconds
        .map(|duration_seconds| CborValue::Map(delegate_detail_entries(None, duration_seconds)))
}

fn delegate_detail_entries(
    output: Option<String>,
    duration_seconds: u64,
) -> Vec<(CborValue, CborValue)> {
    let mut entries = Vec::new();
    if let Some(output) = output {
        entries.push((
            CborValue::Text("output".to_owned()),
            CborValue::Text(output),
        ));
    }
    entries.push((
        CborValue::Text("duration_seconds".to_owned()),
        CborValue::Integer((duration_seconds as i64).into()),
    ));
    entries
}

#[derive(Clone, Debug, PartialEq)]
enum WaitCallState {
    Pending,
    Backgrounded,
    NormalReturned,
    BackgroundResult(ToolBackgroundResult),
    BackgroundError(ToolBackgroundError),
    Consumed,
}

#[derive(Clone, Debug, PartialEq)]
struct WaitRequest {
    call_id: ToolCallId,
    tool_name: ToolName,
}

#[derive(Clone, Debug, PartialEq)]
enum WaitReplyKind {
    Result {
        result: CborValue,
        display: Option<ToolDisplay>,
    },
    Error {
        message: String,
        details: Option<CborValue>,
        display: Option<ToolDisplay>,
    },
}

#[derive(Clone, Debug, PartialEq)]
struct WaitReply {
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    kind: WaitReplyKind,
    suppress_call_id: Option<ToolCallId>,
    unsuppress_call_id: Option<ToolCallId>,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct WaitStart {
    reply: Option<WaitReply>,
    suppress_call_id: Option<ToolCallId>,
}

#[derive(Default)]
struct WaitTracker {
    calls: HashMap<ToolCallId, WaitCallState>,
    waiters: HashMap<ToolCallId, WaitRequest>,
}

impl WaitTracker {
    fn record_tool_invoke(&mut self, call_id: ToolCallId, tool_name: ToolName) {
        if tool_name.as_str() != WAIT_TOOL_NAME {
            self.calls.entry(call_id).or_insert(WaitCallState::Pending);
        }
    }

    fn handle_wait_invoke(
        &mut self,
        call_id: ToolCallId,
        tool_name: ToolName,
        arguments: &CborValue,
    ) -> WaitStart {
        let target = match parse_wait_args(arguments) {
            Ok(target) => target,
            Err(message) => {
                return WaitStart::reply(wait_error_reply(
                    call_id,
                    tool_name,
                    message,
                    Some(arguments.clone()),
                ));
            }
        };
        self.start_wait(target, WaitRequest { call_id, tool_name })
    }

    fn start_wait(&mut self, target: ToolCallId, wait: WaitRequest) -> WaitStart {
        if self.waiters.contains_key(&target) {
            return WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                "existing wait for this tool already in progress".to_owned(),
                None,
            ));
        }
        let state = self.calls.remove(&target);
        match state {
            Some(WaitCallState::Pending) => {
                self.calls.insert(target.clone(), WaitCallState::Pending);
                self.waiters.insert(target, wait);
                WaitStart::default()
            }
            Some(WaitCallState::Backgrounded) => {
                self.calls
                    .insert(target.clone(), WaitCallState::Backgrounded);
                self.waiters.insert(target.clone(), wait);
                WaitStart::suppress(target)
            }
            Some(WaitCallState::NormalReturned) => {
                self.calls.insert(target.clone(), WaitCallState::Consumed);
                WaitStart::reply(wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    format!("Tool call {target} returned normally, not backgrounded"),
                    None,
                ))
            }
            Some(WaitCallState::BackgroundResult(result)) => {
                self.calls.insert(target.clone(), WaitCallState::Consumed);
                WaitStart::reply_with_suppress(
                    wait_result_reply(wait.call_id, wait.tool_name, result.result, result.display),
                    target,
                )
            }
            Some(WaitCallState::BackgroundError(error)) => {
                self.calls.insert(target.clone(), WaitCallState::Consumed);
                WaitStart::reply_with_suppress(
                    wait_error_reply(wait.call_id, wait.tool_name, error.message, error.details)
                        .with_display(error.display),
                    target,
                )
            }
            Some(WaitCallState::Consumed) => WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                format!("result for tool call `{target}` already consumed"),
                None,
            )),
            None => WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                format!("unknown tool call: `{target}`"),
                None,
            )),
        }
    }

    fn record_tool_result(&mut self, result: ToolResult) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        if self.is_consumed(&call_id) || self.is_backgrounded(&call_id) {
            return Vec::new();
        }
        if result.kind == ToolResultKind::BackgroundPlaceholder {
            self.calls.insert(call_id, WaitCallState::Backgrounded);
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id, WaitCallState::Consumed);
            return vec![wait_result_reply(
                wait.call_id,
                wait.tool_name,
                result.result,
                result.display,
            )];
        }
        self.calls.insert(call_id, WaitCallState::NormalReturned);
        Vec::new()
    }

    fn record_tool_error(&mut self, error: ToolError) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id, WaitCallState::Consumed);
            return vec![
                wait_error_reply(wait.call_id, wait.tool_name, error.message, error.details)
                    .with_display(error.display),
            ];
        }
        self.calls.insert(call_id, WaitCallState::NormalReturned);
        Vec::new()
    }

    fn record_background_result(&mut self, result: ToolBackgroundResult) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundResult(result));
        self.resolve_waiter_with_background(&call_id)
    }

    fn record_background_error(&mut self, error: ToolBackgroundError) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundError(error));
        self.resolve_waiter_with_background(&call_id)
    }

    fn resolve_waiter_with_background(&mut self, call_id: &ToolCallId) -> Vec<WaitReply> {
        let Some(wait) = self.waiters.remove(call_id) else {
            return Vec::new();
        };
        let Some(state) = self.calls.remove(call_id) else {
            return Vec::new();
        };
        self.calls.insert(call_id.clone(), WaitCallState::Consumed);
        match state {
            WaitCallState::BackgroundResult(result) => vec![
                wait_result_reply(wait.call_id, wait.tool_name, result.result, result.display)
                    .with_suppress(call_id.clone()),
            ],
            WaitCallState::BackgroundError(error) => vec![
                wait_error_reply(wait.call_id, wait.tool_name, error.message, error.details)
                    .with_display(error.display)
                    .with_suppress(call_id.clone()),
            ],
            _ => Vec::new(),
        }
    }

    fn interrupt_active_waits(&mut self) -> Vec<WaitReply> {
        let waiters = std::mem::take(&mut self.waiters);
        waiters
            .into_iter()
            .map(|(target, wait)| {
                let mut reply = wait_interrupted_reply(wait.call_id, wait.tool_name, &target);
                if self.is_backgrounded(&target) {
                    reply = reply.with_unsuppress(target);
                }
                reply
            })
            .collect()
    }

    fn is_backgrounded(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|state| matches!(state, WaitCallState::Backgrounded))
    }

    fn is_consumed(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|state| matches!(state, WaitCallState::Consumed))
    }
}

impl WaitReply {
    fn with_display(mut self, display: Option<ToolDisplay>) -> Self {
        if let WaitReplyKind::Error { display: dst, .. } = &mut self.kind {
            *dst = display;
        }
        self
    }

    fn with_suppress(mut self, call_id: ToolCallId) -> Self {
        self.suppress_call_id = Some(call_id);
        self
    }

    fn with_unsuppress(mut self, call_id: ToolCallId) -> Self {
        self.unsuppress_call_id = Some(call_id);
        self
    }
}

impl WaitStart {
    fn reply(reply: WaitReply) -> Self {
        Self {
            reply: Some(reply),
            suppress_call_id: None,
        }
    }

    fn suppress(call_id: ToolCallId) -> Self {
        Self {
            reply: None,
            suppress_call_id: Some(call_id),
        }
    }

    fn reply_with_suppress(reply: WaitReply, call_id: ToolCallId) -> Self {
        Self {
            reply: Some(reply),
            suppress_call_id: Some(call_id),
        }
    }
}

fn wait_result_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    result: CborValue,
    display: Option<ToolDisplay>,
) -> WaitReply {
    WaitReply {
        wait_call_id,
        wait_tool_name,
        kind: WaitReplyKind::Result { result, display },
        suppress_call_id: None,
        unsuppress_call_id: None,
    }
}

fn wait_error_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    message: String,
    details: Option<CborValue>,
) -> WaitReply {
    WaitReply {
        wait_call_id,
        wait_tool_name,
        kind: WaitReplyKind::Error {
            message,
            details,
            display: None,
        },
        suppress_call_id: None,
        unsuppress_call_id: None,
    }
}

fn wait_interrupted_reply(
    wait_call_id: ToolCallId,
    wait_tool_name: ToolName,
    target_call_id: &ToolCallId,
) -> WaitReply {
    wait_result_reply(
        wait_call_id,
        wait_tool_name,
        CborValue::Text(format!(
            "{}: true\n\nWaiting for tool call `{target_call_id}` was interrupted because user input is queued. Try again later.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        )),
        None,
    )
}

fn parse_wait_args(arguments: &CborValue) -> Result<ToolCallId, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        if name == "tool_call_id" {
            return match v {
                CborValue::Text(text) => {
                    let text = text.trim();
                    if text.is_empty() {
                        Err("`tool_call_id` must not be empty".to_owned())
                    } else {
                        Ok(text.to_owned().into())
                    }
                }
                _ => Err("`tool_call_id` must be a string".to_owned()),
            };
        }
    }
    Err("missing string argument: tool_call_id".to_owned())
}
