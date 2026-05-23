//! Injection point for harness-internal tools owned by higher crates.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use tau_proto::{
    CborValue, Event, StartAgentRequest, ToolCallId, ToolDisplay, ToolError, ToolName, ToolResult,
    ToolSpec,
};

use crate::discovery::DiscoveredSkillSource;
use crate::error::HarnessError;
use crate::harness::{HARNESS_CONNECTION_ID, Harness};
use crate::{AgentToolCall, ConversationId};

/// A handler for tools implemented inside the harness process.
pub trait InternalToolHandler: Send + Sync {
    /// Tool specifications this handler registers as internal tools.
    fn tool_specs(&self) -> Vec<ToolSpec>;

    /// Return true when this handler owns `internal_tool_name`.
    fn handles(&self, internal_tool_name: &ToolName) -> bool;

    /// React to a committed event.
    ///
    /// Internal tools observe the same durable lifecycle events as external
    /// extensions. A handler should filter for events it owns, such as
    /// `ToolStarted` for its registered tools or later correlation events like
    /// `StartAgentResult`.
    fn handle_event(
        &self,
        host: &mut InternalToolHost<'_>,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let _ = host;
        let _ = event;
        Ok(())
    }
}

/// Shared reference-counted internal tool handler.
pub type InternalToolHandlers = Vec<Arc<dyn InternalToolHandler>>;

/// Public snapshot of one skill known to the harness.
#[derive(Clone)]
pub struct InternalSkill {
    /// Skill name used as the `skill` query exact match.
    pub name: String,
    /// Short human-facing description.
    pub description: String,
    /// Markdown source for loading or content search.
    pub source: InternalSkillSource,
}

/// Public snapshot of a skill Markdown source.
#[derive(Clone)]
pub enum InternalSkillSource {
    /// An extension-announced skill backed by an on-disk Markdown file.
    File(PathBuf),
    /// A Tau built-in skill embedded into the harness binary.
    BuiltIn { content: Cow<'static, str> },
}

impl InternalSkillSource {
    /// Human-readable source label for warnings.
    pub fn label(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::BuiltIn { .. } => "built-in skill".to_owned(),
        }
    }
}

/// Narrow facade exposed to internal tool handler crates.
pub struct InternalToolHost<'a> {
    harness: &'a mut Harness,
}

impl<'a> InternalToolHost<'a> {
    pub(crate) fn new(harness: &'a mut Harness) -> Self {
        Self { harness }
    }

    /// Register a harness-process internal tool.
    pub fn register_internal_tool(&mut self, spec: ToolSpec) {
        let _ = self
            .harness
            .registry
            .register_internal(HARNESS_CONNECTION_ID, spec);
    }

    /// Return a cloned snapshot of skills discovered by the harness.
    pub fn discovered_skills(&self) -> Vec<InternalSkill> {
        self.harness
            .discovered_skills
            .iter()
            .map(|(name, skill)| InternalSkill {
                name: name.as_str().to_owned(),
                description: skill.description.clone(),
                source: match &skill.source {
                    DiscoveredSkillSource::File(path) => InternalSkillSource::File(path.clone()),
                    DiscoveredSkillSource::BuiltIn { content } => InternalSkillSource::BuiltIn {
                        content: content.clone(),
                    },
                },
            })
            .collect()
    }

    /// Emit an important informational message to the user.
    pub fn emit_info_important(&mut self, message: &str) {
        self.harness.emit_info_important(message);
    }

    /// Ensure and return the agent id for a conversation.
    pub fn ensure_agent_id_for_conversation(
        &mut self,
        conversation_id: &ConversationId,
    ) -> Option<String> {
        self.harness
            .ensure_agent_id_for_conversation(conversation_id)
    }

    /// Mint an agent id appropriate for `role`.
    pub fn mint_agent_id_for_role(&self, role: &str) -> String {
        crate::harness::mint_agent_id_for_role(role)
    }

    /// Enqueue a start-agent request from an internal handler without draining.
    pub fn enqueue_start_agent_request_without_draining(
        &mut self,
        query: StartAgentRequest,
    ) -> Result<String, String> {
        self.harness
            .enqueue_internal_start_agent_request_without_draining(query)
    }

    /// Drain queued start-agent requests.
    pub fn drain_start_agent_requests(&mut self) -> Result<(), HarnessError> {
        self.harness.drain_pending_start_agent_requests()
    }

    /// Mark a call as backgrounded for instant-background tools.
    pub fn mark_tool_backgrounded(&mut self, call_id: &ToolCallId) -> bool {
        self.harness.tool_turn.mark_backgrounded(call_id)
    }

    /// Publish a background placeholder with custom text.
    pub fn publish_background_placeholder(&mut self, call_id: &ToolCallId, result: CborValue) {
        self.harness
            .publish_internal_background_placeholder(call_id, result);
    }

    /// Complete a prebuilt internal tool result, routing foreground/background.
    pub fn finish_prebuilt_tool_result(&mut self, result: ToolResult) {
        self.harness.finish_prebuilt_internal_tool_result(result);
    }

    /// Complete a prebuilt internal tool error, routing foreground/background.
    pub fn finish_prebuilt_tool_error(&mut self, error: ToolError) {
        self.harness.finish_prebuilt_internal_tool_error(error);
    }

    /// Handle the built-in `wait` tool.
    pub fn handle_wait_tool_call(
        &mut self,
        conversation_id: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        self.harness
            .handle_wait_tool_call(conversation_id, call, visible_tool_name)
    }

    #[cfg(test)]
    pub(crate) fn handle_message_tool_call(
        &mut self,
        conversation_id: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: ToolName,
    ) -> Result<(), HarnessError> {
        self.harness
            .handle_message_tool_call(conversation_id, call, visible_tool_name)
    }

    /// Resolve a committed `ToolStarted` event for an internal tool.
    pub fn internal_started_call(
        &mut self,
        started: &tau_proto::ToolStarted,
    ) -> Option<(ConversationId, AgentToolCall, ToolName)> {
        let cid = self
            .harness
            .tool_conversations
            .get(&started.call_id)?
            .clone();
        let pending = self.harness.pending_tools.get(&started.call_id)?.clone();
        let call = AgentToolCall {
            id: started.call_id.clone(),
            name: pending.internal_name,
            tool_type: pending.tool_type,
            arguments: started.arguments.clone(),
            display: None,
        };
        Some((cid, call, pending.name))
    }

    /// Ensure the harness tracks an internal tool call before it completes.
    pub fn ensure_internal_tool_tracking(
        &mut self,
        conversation_id: &ConversationId,
        call: &AgentToolCall,
        visible_tool_name: &ToolName,
    ) {
        self.harness
            .ensure_harness_owned_tool_tracking(conversation_id, call, visible_tool_name);
    }

    /// Complete an internal tool call with a final text result.
    pub fn finish_tool_with_result(
        &mut self,
        conversation_id: &ConversationId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        result: String,
        details: Option<tau_proto::CborValue>,
    ) {
        self.harness.finish_harness_owned_tool_with_result(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            result,
            details,
        );
    }

    /// Complete an internal tool call with a final structured result.
    pub fn finish_tool_with_cbor_result(
        &mut self,
        conversation_id: &ConversationId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        result: tau_proto::CborValue,
        display: Option<ToolDisplay>,
    ) {
        self.harness.finish_harness_owned_tool_with_cbor_result(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            result,
            display,
        );
    }

    /// Complete an internal tool call with a final error.
    pub fn finish_tool_with_error(
        &mut self,
        conversation_id: &ConversationId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        message: String,
        details: Option<tau_proto::CborValue>,
    ) {
        self.harness.finish_harness_owned_tool_with_error(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            message,
            details,
        );
    }

    /// Complete an internal tool call with a final displayed error.
    pub fn finish_tool_with_display_error(
        &mut self,
        conversation_id: &ConversationId,
        call_id: tau_proto::ToolCallId,
        tool_name: ToolName,
        tool_type: tau_proto::ToolType,
        message: String,
        details: Option<tau_proto::CborValue>,
        display: Option<ToolDisplay>,
    ) {
        self.harness.finish_harness_owned_tool_with_display_error(
            conversation_id,
            call_id,
            tool_name,
            tool_type,
            message,
            details,
            display,
        );
    }

    /// Return true when a tool call is still tracked as running.
    pub fn is_running_tool_call(&self, target_call_id: &ToolCallId) -> bool {
        self.harness.is_running_tool_call(target_call_id)
    }

    /// Publish a durable broadcast tool cancellation request.
    pub fn publish_tool_cancel_request(&mut self, target_call_id: ToolCallId) {
        self.harness.publish_tool_cancel_request(target_call_id);
    }

    /// Cancel a start-agent request owned by an internal tool handler.
    pub fn cancel_start_agent_request(
        &mut self,
        query_id: &str,
        target_call_id: &ToolCallId,
        suppress_background_completion_prompt: bool,
    ) -> Result<(), String> {
        self.harness.cancel_start_agent_request(
            query_id,
            target_call_id,
            suppress_background_completion_prompt,
        )
    }

    /// Publish an agent-to-agent or agent-to-user message from a conversation.
    pub fn publish_agent_message(
        &mut self,
        conversation_id: &ConversationId,
        recipient_id: String,
        message: String,
    ) -> Result<(), String> {
        self.harness
            .publish_agent_message_from_conversation(conversation_id, recipient_id, message)
    }
}

impl Harness {
    /// Install handlers and register their internal tool specs.
    pub fn install_internal_tool_handlers(&mut self, handlers: InternalToolHandlers) {
        self.internal_tool_handlers = handlers;
        let handlers = self.internal_tool_handlers.clone();
        let mut host = InternalToolHost::new(self);
        for handler in handlers {
            for spec in handler.tool_specs() {
                host.register_internal_tool(spec);
            }
        }
    }

    pub(crate) fn dispatch_internal_tool_event(
        &mut self,
        event: &Event,
    ) -> Result<(), HarnessError> {
        let handlers = self.internal_tool_handlers.clone();
        for handler in handlers {
            let mut host = InternalToolHost::new(self);
            handler.handle_event(&mut host, event)?;
        }
        Ok(())
    }
}
