//! Harness-owned `delegate`, `wait`, `cancel`, and `message` tools.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use tau_proto::{
    AgentMessage, BackgroundSupport, CborValue, Event, SessionContextKey, SessionContextValue,
    ToolBackgroundError, ToolBackgroundResult, ToolCallId, ToolDisplay, ToolDisplayStats,
    ToolError, ToolExecutionMode, ToolName, ToolRequest, ToolResult, ToolResultKind, ToolSpec,
    ToolType,
};

use crate::conversation::ConversationId;
use crate::error::HarnessError;
use crate::harness::{
    AgentMessageRecipientStatus, AgentToolCall, HARNESS_CONNECTION_ID, Harness, PendingTool,
};

/// Model-visible name of the harness-owned delegate tool.
pub(crate) const DELEGATE_TOOL_NAME: &str = "delegate";
/// Model-visible name of the harness-owned wait tool.
pub(crate) const WAIT_TOOL_NAME: &str = "wait";
/// Model-visible name of the harness-owned cancel tool.
pub(crate) const CANCEL_TOOL_NAME: &str = "cancel";
/// Model-visible name of the harness-owned message tool.
pub(crate) const MESSAGE_TOOL_NAME: &str = "message";

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
    /// Agent id of the caller that invoked `delegate`.
    pub(crate) self_agent_id: Option<String>,
    /// Agent id allocated for the side conversation, when it started.
    pub(crate) agent_id: Option<String>,
}

#[derive(Default)]
pub(crate) struct SubagentToolState {
    /// Outstanding harness-owned delegations indexed by query id.
    pub(crate) pending_delegates: HashMap<String, PendingHarnessDelegate>,
    /// Monotonic id used to create delegate query ids.
    pub(crate) next_delegate_query_id: u64,
    /// State used by the wait tool to track background completions.
    wait_tracker: WaitTracker,
    /// Recent delegate tool calls that already received a cancel request.
    pub(crate) canceled_delegates: HashSet<ToolCallId>,
    /// Insertion order for pruning `canceled_delegates`.
    pub(crate) canceled_delegate_order: VecDeque<ToolCallId>,
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
        let _ = self
            .registry
            .register(HARNESS_CONNECTION_ID, cancel_tool_spec());
        let _ = self
            .registry
            .register(HARNESS_CONNECTION_ID, message_tool_spec());
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
            let owner = self.wait_owner_for_call(call_id);
            self.subagents.wait_tracker.record_tool_invoke(
                call_id.clone(),
                tool.name.clone(),
                owner,
            );
        }
    }

    pub(crate) fn record_wait_tool_result(&mut self, result: ToolResult) {
        let owner = self.wait_owner_for_call(&result.call_id);
        let replies = self
            .subagents
            .wait_tracker
            .record_tool_result(result, owner);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_tool_error(&mut self, error: ToolError) {
        let owner = self.wait_owner_for_call(&error.call_id);
        let replies = self.subagents.wait_tracker.record_tool_error(error, owner);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_background_result(&mut self, result: ToolBackgroundResult) {
        let owner = self.wait_owner_for_call(&result.call_id);
        let replies = self
            .subagents
            .wait_tracker
            .record_background_result(result, owner);
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_background_error(&mut self, error: ToolBackgroundError) {
        let owner = self.wait_owner_for_call(&error.call_id);
        let replies = self
            .subagents
            .wait_tracker
            .record_background_error(error, owner);
        self.publish_wait_replies(replies);
    }

    /// Move the wait tracker's background-call ownership during
    /// side-conversation teardown.
    pub(crate) fn transfer_wait_background_owner_before_teardown(
        &mut self,
        call_id: &ToolCallId,
        source: &ConversationId,
        target: &ConversationId,
    ) {
        self.subagents
            .wait_tracker
            .transfer_call_owner(call_id, source, target);
    }

    fn wait_owner_for_call(&self, call_id: &ToolCallId) -> ConversationId {
        self.tool_conversations
            .get(call_id)
            .or_else(|| self.background_completion_targets.get(call_id))
            .cloned()
            .unwrap_or_else(|| self.default_conversation_id.clone())
    }

    pub(crate) fn interrupt_active_waits(&mut self) {
        let replies = self.subagents.wait_tracker.interrupt_active_waits();
        self.publish_wait_replies(replies);
    }

    pub(crate) fn record_wait_tool_cancelled(&mut self, call_ids: &HashSet<ToolCallId>) {
        let cancelled = self.subagents.wait_tracker.record_tool_cancelled(call_ids);
        for call_id in cancelled.unsuppress_call_ids {
            self.unsuppress_background_completion_prompt(call_id);
        }
        self.publish_wait_replies(cancelled.replies);
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

        let self_agent_id = match self.ensure_agent_id_for_conversation(cid) {
            Some(agent_id) => agent_id,
            None => {
                self.finish_harness_owned_tool_with_error(
                    cid,
                    call_id,
                    visible_tool_name,
                    call.tool_type,
                    "sender conversation no longer exists".to_owned(),
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
                tool_name: visible_tool_name.clone(),
                started_at: Instant::now(),
                self_agent_id: Some(self_agent_id.clone()),
                agent_id: None,
            },
        );
        let start_request = tau_proto::StartAgentRequest {
            query_id: query_id.clone(),
            instruction: format!("{DELEGATE_PREFIX}{}", parsed.prompt),
            role: parsed.role,
            execution_mode: parsed.execution_mode,
            input_stats: ToolDisplayStats::for_text(&parsed.prompt),
            tool_call_id: Some(call_id.clone()),
            task_name: Some(parsed.task_name),
        };
        let agent_id = match self
            .enqueue_harness_delegate_start_agent_request_without_draining(start_request)
        {
            Ok(agent_id) => agent_id,
            Err(message) => {
                self.subagents.pending_delegates.remove(&query_id);
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
        if let Some(pending) = self.subagents.pending_delegates.get_mut(&query_id) {
            pending.agent_id = Some(agent_id.clone());
        }
        if self.tool_turn.mark_backgrounded(&call_id) {
            self.publish_synthetic_background_result_with_agent_ids(
                &call_id,
                &self_agent_id,
                &agent_id,
            );
        }
        // `delegate` is harness-owned and already inside the main event loop, so
        // publishing a bus event would only echo an internal command. Use the
        // same shared scheduler that external `agent.start_request` events reach.
        self.drain_pending_start_agent_requests()
    }

    /// Handle the harness-owned `message` tool call inline.
    pub(crate) fn handle_message_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        self.track_harness_owned_tool_request(cid, call, &visible_tool_name);
        let result = parse_message_args(&call.arguments).and_then(|parsed| {
            let sender_id = self
                .ensure_agent_id_for_conversation(cid)
                .ok_or_else(|| "sender conversation no longer exists".to_owned())?;
            if parsed.recipient_id != "user" {
                match self.agent_message_recipient_status(&parsed.recipient_id) {
                    AgentMessageRecipientStatus::Live => {}
                    AgentMessageRecipientStatus::Stopped => {
                        return Err(format!(
                            "stopped message recipient: `{}`",
                            parsed.recipient_id
                        ));
                    }
                    AgentMessageRecipientStatus::Unknown => {
                        return Err(format!(
                            "unknown message recipient: `{}`",
                            parsed.recipient_id
                        ));
                    }
                }
            }
            let session_id = self
                .conversations
                .get(cid)
                .map(|conv| conv.session_id.clone())
                .unwrap_or_else(|| self.current_session_id.clone());
            self.publish_event(
                Some(HARNESS_CONNECTION_ID),
                Event::AgentMessage(AgentMessage {
                    session_id,
                    sender_id,
                    recipient_id: parsed.recipient_id,
                    message: parsed.message,
                }),
            );
            Ok(())
        });
        match result {
            Ok(()) => self.finish_harness_owned_tool_with_result(
                cid,
                call_id,
                visible_tool_name,
                call.tool_type,
                "Message sent".to_owned(),
                None,
            ),
            Err(message) => self.finish_harness_owned_tool_with_error(
                cid,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            ),
        }
        Ok(())
    }

    /// Handle the harness-owned `cancel` tool call inline.
    pub(crate) fn handle_cancel_tool_call(
        &mut self,
        cid: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        let call_id: ToolCallId = call.id.clone();
        self.track_harness_owned_tool_request(cid, call, &visible_tool_name);
        let result = match parse_cancel_args(&call.arguments) {
            Ok(target_call_id) => self.cancel_tool_call(&target_call_id),
            Err(message) => Err(message),
        };
        match result {
            Ok(()) => self.finish_harness_owned_tool_with_result(
                cid,
                call_id,
                visible_tool_name,
                call.tool_type,
                "Tool cancellation sent".to_owned(),
                None,
            ),
            Err(message) => self.finish_harness_owned_tool_with_error(
                cid,
                call_id,
                visible_tool_name,
                call.tool_type,
                message,
                Some(call.arguments.clone()),
            ),
        }
        Ok(())
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
            cid,
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
        cid: &ConversationId,
        query_id: &str,
        text: String,
        error: Option<String>,
    ) {
        self.complete_harness_delegate_inner(cid, query_id, text, error, false);
    }

    pub(crate) fn complete_harness_delegate_inner(
        &mut self,
        _cid: &ConversationId,
        query_id: &str,
        text: String,
        error: Option<String>,
        suppress_background_completion_prompt: bool,
    ) {
        let Some(pending) = self.subagents.pending_delegates.remove(query_id) else {
            return;
        };
        let duration_seconds = delegate_duration_seconds(pending.started_at.elapsed());
        let self_agent_id = pending.self_agent_id.clone();
        let agent_id = pending.agent_id.clone();
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
                details: delegate_error_details(
                    duration_seconds,
                    self_agent_id.as_deref(),
                    agent_id.as_deref(),
                ),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            };
            if self.tool_turn.is_backgrounded(&call_id) {
                if suppress_background_completion_prompt {
                    self.handle_background_tool_error_without_completion_prompt(
                        Some(HARNESS_CONNECTION_ID),
                        event,
                    );
                } else {
                    self.handle_background_tool_error(Some(HARNESS_CONNECTION_ID), event);
                }
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
                result: delegate_result_value(
                    text,
                    duration_seconds,
                    self_agent_id.as_deref(),
                    agent_id.as_deref(),
                ),
                kind: ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            };
            if self.tool_turn.is_backgrounded(&call_id) {
                self.handle_background_tool_result(HARNESS_CONNECTION_ID, event);
            } else {
                self.publish_terminal_tool_result(Some(&owner_cid), None, event);
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
                internal_name: visible_tool_name.clone(),
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

    fn finish_harness_owned_tool_with_result(
        &mut self,
        cid: &ConversationId,
        call_id: ToolCallId,
        tool_name: ToolName,
        tool_type: ToolType,
        result: String,
        details: Option<CborValue>,
    ) {
        let result = ToolResult {
            call_id: call_id.clone(),
            tool_name,
            tool_type,
            result: details.unwrap_or(CborValue::Text(result)),
            kind: ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        };
        self.publish_terminal_tool_result(Some(cid), None, result);
        self.on_tool_call_complete(call_id.as_str());
        self.clear_tool_call_tracking(call_id.as_str());
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
            match reply.kind {
                WaitReplyKind::Result { result, display } => {
                    self.publish_terminal_tool_result(
                        Some(&cid),
                        None,
                        ToolResult {
                            call_id: reply.wait_call_id,
                            tool_name: reply.wait_tool_name,
                            tool_type: ToolType::Function,
                            result,
                            kind: ToolResultKind::Final,
                            display,
                            originator: tau_proto::PromptOriginator::User,
                        },
                    );
                }
                WaitReplyKind::Error {
                    message,
                    details,
                    display,
                } => {
                    self.publish_terminal_tool_error(
                        Some(&cid),
                        None,
                        ToolError {
                            call_id: reply.wait_call_id,
                            tool_name: reply.wait_tool_name,
                            tool_type: ToolType::Function,
                            message,
                            details,
                            display,
                            originator: tau_proto::PromptOriginator::User,
                        },
                    );
                }
            }
            self.on_tool_call_complete(wait_call_id.as_str());
            self.clear_tool_call_tracking(wait_call_id.as_str());
        }
    }
}

fn delegate_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(DELEGATE_TOOL_NAME),
        model_visible_name: None,
        description: Some("Delegate a self-contained sub-task to a fresh sub-agent that runs with its own context and tools, and returns only its final text answer. The instant background placeholder and final result include `self_agent_id` and `sub_agent_id` headers/values. Pass `sub_agent_id` to `message`. Use it for: open-ended exploration where step count is unpredictable; large search/read sweeps whose intermediate output would otherwise clutter this conversation; parallel work — multiple delegations with `execution_mode: \"shared\"` or compatible `execution_mode: \"update\"` can overlap globally. Use `execution_mode: \"update\"` when the sub-agent may update shared state and should not overlap with another update or exclusive sub-agent. Use `execution_mode: \"exclusive\"` when the sub-agent needs to run alone: it waits for all other sub-agent delegations and blocks later independent ones until it finishes. Skip it when the target is already known (use direct tools like `read`/`grep`/`shell` instead) or when the task requires synthesis you should do yourself — don't push 'based on findings, fix the bug' onto a sub-agent; investigate first, then delegate the concrete change. The sub-agent starts with a *clean* conversation: it sees ONLY your `prompt`, plus its tools and system prompt. It cannot see this conversation's prior turns, your reasoning, files you've read, or earlier tool results — and that isolation applies at every nesting depth, so a sub-agent's own delegations are equally fresh. You must therefore brief the sub-agent fully: state the goal, hand it every fact it needs (absolute file paths, exact symbols, code snippets, prior findings, constraints, format of the answer you want), and frame the sub-task as if writing to a teammate who just walked into the room. Terse command-style prompts produce shallow, generic work; missing context produces wrong answers.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "task_name": { "type": "string", "description": "Short human-readable label for the sub-task (a few words, lowercase). Surfaced live to the user as `delegate [task_name]` while the sub-agent runs." },
                "prompt": { "type": "string", "description": "Self-contained task for the sub-agent. The sub-agent's conversation starts fresh — it has NO access to this conversation's history, your earlier tool results, or files you've read. State everything it needs: the goal, the relevant facts (absolute file paths, exact symbols, snippets you've already extracted), any constraints, what counts as 'done', and the format of the answer you want back. Treat it like briefing a teammate who just walked into the room. Terse command-style prompts produce shallow work; missing context produces wrong answers." },
                "execution_mode": { "type": "string", "enum": ["shared", "update", "exclusive"], "description": "Use `shared` when the sub-task can safely overlap globally with other shared/update sub-agent delegations. Use `update` when it may change shared state: it can overlap with shared sub-agents, but not update or exclusive ones. Use `exclusive` when it must run alone: it waits for all other sub-agent delegations and blocks later independent ones. Default: `shared`." },
                "role": { "type": "string", "description": "Optional sub-agent role to use. When omitted, Tau defaults delegate calls to `engineer` if that role is available and enabled." }
            },
            "required": ["task_name", "prompt"],
            "additionalProperties": false
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Instant),
    }
}

fn message_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(MESSAGE_TOOL_NAME),
        model_visible_name: None,
        description: Some("Send an async message to another live or pending agent, or to the user. Use recipient_id `user`, or a `sub_agent_id` returned by `delegate`; UI display depends on `/set show-messages`. A non-user recipient also receives a hidden prompt. Requires `recipient_id` and `message`.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "recipient_id": { "type": "string", "description": "Recipient agent_id, or the special value `user`." },
                "message": { "type": "string", "description": "Message body." }
            },
            "required": ["recipient_id", "message"],
            "additionalProperties": false
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Never),
    }
}

fn cancel_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(CANCEL_TOOL_NAME),
        model_visible_name: None,
        description: Some("Cancel a running supported background tool call. Requires `tool_call_id`; currently delegate and shell tool calls can be canceled. Duplicate cancellation requests for the same tool call fail when tracked.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": { "tool_call_id": { "type": "string", "description": "Required id of the running supported tool call to cancel." } },
            "required": ["tool_call_id"],
            "additionalProperties": false
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Never),
    }
}

fn wait_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new(WAIT_TOOL_NAME),
        model_visible_name: None,
        description: Some("Wait for background tool calls. With `tool_call_id`, wait for that specific background call. Without `tool_call_id`, wait for the first background call in this conversation to finish and return its `original_tool_call_id`. Already-finished matching results return immediately. Tau will notify you via marked internal messages about background calls completing; `wait({})` consumes one completion and suppresses that completion notice.".to_owned()),
        tool_type: ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": { "tool_call_id": { "type": "string", "description": "Optional. When set, wait for this specific background tool call. When omitted, wait for the first background tool call in this conversation to finish." } },
            "additionalProperties": false
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Never),
    }
}

#[derive(Debug)]
struct DelegateArgs {
    task_name: String,
    prompt: String,
    execution_mode: ToolExecutionMode,
    role: Option<String>,
}

#[derive(Debug, PartialEq)]
struct MessageArgs {
    recipient_id: String,
    message: String,
}

fn parse_message_args(arguments: &CborValue) -> Result<MessageArgs, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    let mut recipient_id = None;
    let mut message = None;
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        match name.as_str() {
            "recipient_id" => match v {
                CborValue::Text(text) => recipient_id = Some(text.clone()),
                _ => return Err("`recipient_id` must be a string".to_owned()),
            },
            "message" => match v {
                CborValue::Text(text) => message = Some(text.clone()),
                _ => return Err("`message` must be a string".to_owned()),
            },
            _ => {}
        }
    }
    let recipient_id = recipient_id.ok_or_else(|| "`recipient_id` is required".to_owned())?;
    if recipient_id.trim().is_empty() {
        return Err("`recipient_id` must not be empty".to_owned());
    }
    let message = message.ok_or_else(|| "`message` is required".to_owned())?;
    if message.trim().is_empty() {
        return Err("`message` must not be empty".to_owned());
    }
    Ok(MessageArgs {
        recipient_id,
        message,
    })
}

fn parse_cancel_args(arguments: &CborValue) -> Result<ToolCallId, String> {
    let CborValue::Map(entries) = arguments else {
        return Err("arguments must be an object".to_owned());
    };
    for (k, v) in entries {
        let CborValue::Text(name) = k else { continue };
        if name == "tool_call_id" {
            return match v {
                CborValue::Text(text) if !text.is_empty() => Ok(text.clone().into()),
                CborValue::Text(_) => Err("`tool_call_id` must not be empty".to_owned()),
                _ => Err("`tool_call_id` must be a string".to_owned()),
            };
        }
    }
    Err("`tool_call_id` is required".to_owned())
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
                CborValue::Text(text) if text == "update" => {
                    execution_mode = Some(ToolExecutionMode::Update)
                }
                CborValue::Text(text) if text == "exclusive" => {
                    execution_mode = Some(ToolExecutionMode::Exclusive)
                }
                CborValue::Text(_) => {
                    return Err(
                        "`execution_mode` must be `shared`, `update`, or `exclusive`".to_owned(),
                    );
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

fn delegate_result_value(
    text: String,
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> CborValue {
    if duration_seconds.is_none() && self_agent_id.is_none() && agent_id.is_none() {
        return CborValue::Text(text);
    }
    CborValue::Map(delegate_detail_entries(
        Some(text),
        duration_seconds,
        self_agent_id,
        agent_id,
    ))
}

fn delegate_error_details(
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> Option<CborValue> {
    if duration_seconds.is_none() && self_agent_id.is_none() && agent_id.is_none() {
        return None;
    }
    Some(CborValue::Map(delegate_detail_entries(
        None,
        duration_seconds,
        self_agent_id,
        agent_id,
    )))
}

fn delegate_detail_entries(
    output: Option<String>,
    duration_seconds: Option<u64>,
    self_agent_id: Option<&str>,
    agent_id: Option<&str>,
) -> Vec<(CborValue, CborValue)> {
    let mut entries = Vec::new();
    if let Some(self_agent_id) = self_agent_id {
        entries.push((
            CborValue::Text("self_agent_id".to_owned()),
            CborValue::Text(self_agent_id.to_owned()),
        ));
    }
    if let Some(agent_id) = agent_id {
        entries.push((
            CborValue::Text("sub_agent_id".to_owned()),
            CborValue::Text(agent_id.to_owned()),
        ));
    }
    if let Some(duration_seconds) = duration_seconds {
        entries.push((
            CborValue::Text("duration_seconds".to_owned()),
            CborValue::Integer((duration_seconds as i64).into()),
        ));
    }
    if let Some(output) = output {
        entries.push((
            CborValue::Text("output".to_owned()),
            CborValue::Text(output),
        ));
    }
    entries
}

const ORIGINAL_TOOL_CALL_ID_HEADER: &str = "original_tool_call_id";

#[derive(Clone, Debug, PartialEq)]
enum WaitTarget {
    Exact(ToolCallId),
    AnyBackground,
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

#[derive(Clone, Debug, PartialEq, Default)]
struct WaitCancel {
    replies: Vec<WaitReply>,
    unsuppress_call_ids: Vec<ToolCallId>,
}

#[derive(Default)]
struct WaitTracker {
    calls: HashMap<ToolCallId, WaitCallState>,
    waiters: HashMap<ToolCallId, WaitRequest>,
    any_waiters: HashMap<ConversationId, WaitRequest>,
    call_owners: HashMap<ToolCallId, ConversationId>,
    completion_order: VecDeque<ToolCallId>,
}

impl WaitTracker {
    fn record_tool_invoke(
        &mut self,
        call_id: ToolCallId,
        tool_name: ToolName,
        owner: ConversationId,
    ) {
        if tool_name.as_str() != WAIT_TOOL_NAME {
            self.call_owners.insert(call_id.clone(), owner);
            self.calls.entry(call_id).or_insert(WaitCallState::Pending);
        }
    }

    fn handle_wait_invoke(
        &mut self,
        owner: &ConversationId,
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
        let wait = WaitRequest { call_id, tool_name };
        match target {
            WaitTarget::Exact(target) => self.start_exact_wait(target, wait),
            WaitTarget::AnyBackground => self.start_any_wait(owner.clone(), wait),
        }
    }

    fn start_exact_wait(&mut self, target: ToolCallId, wait: WaitRequest) -> WaitStart {
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
                self.remove_completed(&target);
                WaitStart::reply_with_suppress(
                    wait_result_reply(wait.call_id, wait.tool_name, result.result, result.display),
                    target,
                )
            }
            Some(WaitCallState::BackgroundError(error)) => {
                self.calls.insert(target.clone(), WaitCallState::Consumed);
                self.remove_completed(&target);
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

    fn start_any_wait(&mut self, owner: ConversationId, wait: WaitRequest) -> WaitStart {
        if self.any_waiters.contains_key(&owner) {
            return WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                "existing wait for a background tool call in this conversation already in progress"
                    .to_owned(),
                None,
            ));
        }
        if let Some(target) = self.oldest_completed_for_owner(&owner) {
            return self.consume_completed_for_any(target, wait);
        }
        if self.has_running_background_for_owner(&owner) {
            self.any_waiters.insert(owner, wait);
            return WaitStart::default();
        }
        WaitStart::reply(wait_error_reply(
            wait.call_id,
            wait.tool_name,
            "no background tool calls are running or completed in this conversation".to_owned(),
            None,
        ))
    }

    fn consume_completed_for_any(&mut self, target: ToolCallId, wait: WaitRequest) -> WaitStart {
        let Some(state) = self.calls.remove(&target) else {
            return WaitStart::reply(wait_error_reply(
                wait.call_id,
                wait.tool_name,
                format!("unknown tool call: `{target}`"),
                None,
            ));
        };
        self.calls.insert(target.clone(), WaitCallState::Consumed);
        self.remove_completed(&target);
        match state {
            WaitCallState::BackgroundResult(result) => WaitStart::reply_with_suppress(
                wait_result_reply(
                    wait.call_id,
                    wait.tool_name,
                    result_with_original_tool_call_id(&target, result.result),
                    result.display,
                ),
                target,
            ),
            WaitCallState::BackgroundError(error) => WaitStart::reply_with_suppress(
                wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    error.message,
                    details_with_original_tool_call_id(&target, error.details),
                )
                .with_display(error.display),
                target,
            ),
            other => {
                self.calls.insert(target.clone(), other);
                WaitStart::reply(wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    format!("tool call `{target}` has no completed background result"),
                    None,
                ))
            }
        }
    }

    fn record_tool_result(&mut self, result: ToolResult, owner: ConversationId) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        self.call_owners.insert(call_id.clone(), owner);
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

    fn record_tool_error(&mut self, error: ToolError, owner: ConversationId) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        self.call_owners.insert(call_id.clone(), owner);
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

    fn record_background_result(
        &mut self,
        result: ToolBackgroundResult,
        owner: ConversationId,
    ) -> Vec<WaitReply> {
        if result.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = result.call_id.clone();
        self.call_owners.insert(call_id.clone(), owner.clone());
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let mut replies = vec![
                wait_result_reply(wait.call_id, wait.tool_name, result.result, result.display)
                    .with_suppress(call_id.clone()),
            ];
            replies.extend(self.finish_any_waiter_if_no_candidates(&owner));
            return replies;
        }
        if let Some(wait) = self.any_waiters.remove(&owner) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            return vec![
                wait_result_reply(
                    wait.call_id,
                    wait.tool_name,
                    result_with_original_tool_call_id(&call_id, result.result),
                    result.display,
                )
                .with_suppress(call_id),
            ];
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundResult(result));
        self.push_completed(call_id);
        Vec::new()
    }

    fn record_background_error(
        &mut self,
        error: ToolBackgroundError,
        owner: ConversationId,
    ) -> Vec<WaitReply> {
        if error.tool_name.as_str() == WAIT_TOOL_NAME {
            return Vec::new();
        }
        let call_id = error.call_id.clone();
        self.call_owners.insert(call_id.clone(), owner.clone());
        if self.is_consumed(&call_id) {
            return Vec::new();
        }
        if let Some(wait) = self.waiters.remove(&call_id) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            let mut replies = vec![
                wait_error_reply(wait.call_id, wait.tool_name, error.message, error.details)
                    .with_display(error.display)
                    .with_suppress(call_id.clone()),
            ];
            replies.extend(self.finish_any_waiter_if_no_candidates(&owner));
            return replies;
        }
        if let Some(wait) = self.any_waiters.remove(&owner) {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(&call_id);
            return vec![
                wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    error.message,
                    details_with_original_tool_call_id(&call_id, error.details),
                )
                .with_display(error.display)
                .with_suppress(call_id),
            ];
        }
        self.calls
            .insert(call_id.clone(), WaitCallState::BackgroundError(error));
        self.push_completed(call_id);
        Vec::new()
    }

    fn record_tool_cancelled(&mut self, call_ids: &HashSet<ToolCallId>) -> WaitCancel {
        if call_ids.is_empty() {
            return WaitCancel::default();
        }

        let cancelled_owners: HashSet<ConversationId> = call_ids
            .iter()
            .filter_map(|call_id| self.call_owners.get(call_id).cloned())
            .collect();
        let mut cancelled = WaitCancel::default();
        let waiters = std::mem::take(&mut self.waiters);
        for (target, wait) in waiters {
            let target_cancelled = call_ids.contains(&target);
            let wait_cancelled = call_ids.contains(&wait.call_id);
            let target_was_backgrounded = self.is_backgrounded(&target);

            if wait_cancelled {
                if target_was_backgrounded {
                    cancelled.unsuppress_call_ids.push(target.clone());
                }
                continue;
            }
            if target_cancelled {
                let mut reply = wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    format!("Tool call `{target}` was cancelled"),
                    None,
                );
                if target_was_backgrounded {
                    reply = reply.with_unsuppress(target.clone());
                }
                cancelled.replies.push(reply);
            } else {
                self.waiters.insert(target, wait);
            }
        }

        for call_id in call_ids {
            self.calls.insert(call_id.clone(), WaitCallState::Consumed);
            self.remove_completed(call_id);
        }

        let any_waiters = std::mem::take(&mut self.any_waiters);
        for (owner, wait) in any_waiters {
            if call_ids.contains(&wait.call_id) {
                continue;
            }
            if self.oldest_completed_for_owner(&owner).is_some()
                || self.has_running_background_for_owner(&owner)
            {
                self.any_waiters.insert(owner, wait);
            } else if cancelled_owners.contains(&owner) {
                cancelled.replies.push(wait_error_reply(
                    wait.call_id,
                    wait.tool_name,
                    "background tool call in this conversation was cancelled".to_owned(),
                    None,
                ));
            } else {
                self.any_waiters.insert(owner, wait);
            }
        }

        cancelled
    }

    fn interrupt_active_waits(&mut self) -> Vec<WaitReply> {
        let waiters = std::mem::take(&mut self.waiters);
        let mut replies: Vec<WaitReply> = waiters
            .into_iter()
            .map(|(target, wait)| {
                let mut reply = wait_interrupted_reply(wait.call_id, wait.tool_name, &target);
                if self.is_backgrounded(&target) {
                    reply = reply.with_unsuppress(target);
                }
                reply
            })
            .collect();
        replies.extend(
            std::mem::take(&mut self.any_waiters)
                .into_values()
                .map(|wait| wait_interrupted_any_reply(wait.call_id, wait.tool_name)),
        );
        replies
    }

    fn transfer_call_owner(
        &mut self,
        call_id: &ToolCallId,
        source: &ConversationId,
        target: &ConversationId,
    ) {
        if !self.calls.contains_key(call_id) {
            return;
        }
        match self.call_owners.get(call_id) {
            Some(owner) if owner != source => {}
            _ => {
                self.call_owners.insert(call_id.clone(), target.clone());
            }
        }
    }

    fn finish_any_waiter_if_no_candidates(&mut self, owner: &ConversationId) -> Vec<WaitReply> {
        if self.oldest_completed_for_owner(owner).is_some()
            || self.has_running_background_for_owner(owner)
        {
            return Vec::new();
        }
        let Some(wait) = self.any_waiters.remove(owner) else {
            return Vec::new();
        };
        vec![wait_error_reply(
            wait.call_id,
            wait.tool_name,
            "no background tool calls are running or completed in this conversation".to_owned(),
            None,
        )]
    }

    fn oldest_completed_for_owner(&self, owner: &ConversationId) -> Option<ToolCallId> {
        self.completion_order.iter().find_map(|call_id| {
            (self.call_owners.get(call_id) == Some(owner) && self.is_completed(call_id))
                .then_some(call_id.clone())
        })
    }

    fn has_running_background_for_owner(&self, owner: &ConversationId) -> bool {
        self.calls.iter().any(|(call_id, state)| {
            matches!(state, WaitCallState::Backgrounded)
                && self.call_owners.get(call_id) == Some(owner)
        })
    }

    fn push_completed(&mut self, call_id: ToolCallId) {
        if self
            .completion_order
            .iter()
            .all(|existing| existing != &call_id)
        {
            self.completion_order.push_back(call_id);
        }
    }

    fn remove_completed(&mut self, call_id: &ToolCallId) {
        self.completion_order.retain(|existing| existing != call_id);
    }

    fn is_backgrounded(&self, call_id: &ToolCallId) -> bool {
        self.calls
            .get(call_id)
            .is_some_and(|state| matches!(state, WaitCallState::Backgrounded))
    }

    fn is_completed(&self, call_id: &ToolCallId) -> bool {
        self.calls.get(call_id).is_some_and(|state| {
            matches!(
                state,
                WaitCallState::BackgroundResult(_) | WaitCallState::BackgroundError(_)
            )
        })
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

fn wait_interrupted_any_reply(wait_call_id: ToolCallId, wait_tool_name: ToolName) -> WaitReply {
    wait_result_reply(
        wait_call_id,
        wait_tool_name,
        CborValue::Text(format!(
            "{}: true\n\nWaiting for a background tool call in this conversation was interrupted because user input is queued. Try again later.",
            tau_proto::TAU_INTERNAL_HEADER_NAME
        )),
        None,
    )
}

fn result_with_original_tool_call_id(
    original_call_id: &ToolCallId,
    result: CborValue,
) -> CborValue {
    let header = original_tool_call_id_entry(original_call_id);
    match result {
        CborValue::Map(mut entries) => {
            entries.insert(0, header);
            CborValue::Map(entries)
        }
        other => CborValue::Map(vec![header, (CborValue::Text("output".to_owned()), other)]),
    }
}

fn details_with_original_tool_call_id(
    original_call_id: &ToolCallId,
    details: Option<CborValue>,
) -> Option<CborValue> {
    let header = original_tool_call_id_entry(original_call_id);
    Some(match details {
        Some(CborValue::Map(mut entries)) => {
            entries.insert(0, header);
            CborValue::Map(entries)
        }
        Some(other) => CborValue::Map(vec![header, (CborValue::Text("details".to_owned()), other)]),
        None => CborValue::Map(vec![header]),
    })
}

fn original_tool_call_id_entry(original_call_id: &ToolCallId) -> (CborValue, CborValue) {
    (
        CborValue::Text(ORIGINAL_TOOL_CALL_ID_HEADER.to_owned()),
        CborValue::Text(original_call_id.to_string()),
    )
}

fn parse_wait_args(arguments: &CborValue) -> Result<WaitTarget, String> {
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
                        Ok(WaitTarget::Exact(text.to_owned().into()))
                    }
                }
                _ => Err("`tool_call_id` must be a string".to_owned()),
            };
        }
    }
    Ok(WaitTarget::AnyBackground)
}

#[cfg(test)]
mod tests;
