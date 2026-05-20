//! Core delegate tool extension.
//!
//! Registers `delegate`. A tool invocation is translated into an
//! `ExtAgentQuery` side conversation. When the harness returns the
//! side agent's final text, that text is returned as the tool result.

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::time::{Duration, Instant};

use tau_proto::{
    Ack, BackgroundSupport, CborValue, Event, ExtAgentQuery, ExtSessionContextPublish,
    ExtensionContextReady, Frame, FrameReader, FrameWriter, HarnessRolesAvailable, LogEventId,
    Message, SessionContextKey, SessionContextValue, SessionStarted, ToolBackgroundError,
    ToolBackgroundNotificationSuppress, ToolBackgroundResult, ToolCallId, ToolDisplay,
    ToolDisplayStats, ToolError, ToolExecutionMode, ToolInvoke, ToolName, ToolResult,
    ToolResultKind, ToolSpec,
};

pub const LOG_TARGET: &str = "core-subagents";
pub const DELEGATE_TOOL_NAME: &str = "delegate";
pub const TOOL_NAME: &str = DELEGATE_TOOL_NAME;
pub const WAIT_TOOL_NAME: &str = "wait";

const DELEGATE_PREFIX: &str = include_str!("../prompts/delegate_prefix.md");
const SLOW_DELEGATE_EXEC_TIME_THRESHOLD_SECS: u64 = 5;

pub fn run_stdio() -> Result<(), Box<dyn Error>> {
    tau_extension::init_logging_for(LOG_TARGET);
    run(std::io::stdin(), std::io::stdout())
}

pub fn run<R, W>(reader: R, writer: W) -> Result<(), Box<dyn Error>>
where
    R: Read,
    W: Write,
{
    let mut reader = FrameReader::new(BufReader::new(reader));
    let mut writer = FrameWriter::new(BufWriter::new(writer));

    tau_extension::Handshake::tool("tau-ext-core-subagents")
        .subscribe([
            tau_proto::EventName::TOOL_INVOKE,
            tau_proto::EventName::TOOL_RESULT,
            tau_proto::EventName::TOOL_ERROR,
            tau_proto::EventName::TOOL_BACKGROUND_RESULT,
            tau_proto::EventName::TOOL_BACKGROUND_ERROR,
            tau_proto::EventName::TOOL_BACKGROUND_NOTIFICATION_SUPPRESS,
            tau_proto::EventName::EXTENSION_AGENT_QUERY_RESULT,
            tau_proto::EventName::SESSION_STARTED,
            tau_proto::EventName::HARNESS_ROLES_AVAILABLE,
        ])
        .register_tools([delegate_tool_spec(), wait_tool_spec()])
        .ready_message("core-subagents ready")
        .run(&mut writer)?;
    tracing::info!(target: LOG_TARGET, tool = TOOL_NAME, "registered and ready");

    let mut state = RunState::default();

    while let Some(frame) = reader.read_frame()? {
        let (log_id, inner) = frame.peel_log();
        if !handle_frame(inner, &mut state, &mut writer)? {
            break;
        }
        if let Some(id) = log_id {
            ack_log_event(id, &mut writer)?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct RunState {
    /// Outstanding delegations indexed by query id.
    ///
    /// Cleanup relies on the harness invariant that every `ExtAgentQuery` is
    /// answered by exactly one terminal `ExtAgentQueryResult`.
    pending: HashMap<String, PendingDelegate>,
    next_query_id: u64,
    current_session_id: Option<tau_proto::SessionId>,
    latest_roles: Vec<serde_json::Value>,
    wait_tracker: WaitTracker,
}

struct PendingDelegate {
    call_id: ToolCallId,
    tool_name: ToolName,
    started_at: Instant,
}

fn handle_frame<W: Write>(
    frame: Frame,
    state: &mut RunState,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<bool, Box<dyn Error>> {
    match frame {
        Frame::Event(event) => handle_event(event, state, writer)?,
        Frame::Message(Message::Disconnect(_)) => {
            tracing::info!(target: LOG_TARGET, "disconnect received, exiting");
            return Ok(false);
        }
        // No configuration today. `Configure` (and any other message variants
        // the harness may add) is intentionally ignored — the harness does not
        // require an acknowledgement.
        _ => {}
    }
    Ok(true)
}

fn handle_event<W: Write>(
    event: Event,
    state: &mut RunState,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    match event {
        Event::ToolInvoke(invoke) => handle_tool_invoke(
            invoke,
            &mut state.pending,
            &mut state.next_query_id,
            &mut state.wait_tracker,
            writer,
        ),
        Event::ToolResult(result) => {
            write_wait_replies(state.wait_tracker.record_tool_result(result), writer)
        }
        Event::ToolError(error) => {
            write_wait_replies(state.wait_tracker.record_tool_error(error), writer)
        }
        Event::ToolBackgroundResult(result) => {
            write_wait_replies(state.wait_tracker.record_background_result(result), writer)
        }
        Event::ToolBackgroundError(error) => {
            write_wait_replies(state.wait_tracker.record_background_error(error), writer)
        }
        Event::ExtAgentQueryResult(result) => handle_ext_agent_query_result(result, state, writer),
        Event::SessionStarted(SessionStarted { session_id, .. }) => {
            state.current_session_id = Some(session_id.clone());
            publish_delegate_roles_context_and_ready(writer, session_id, &state.latest_roles)
        }
        Event::HarnessRolesAvailable(HarnessRolesAvailable { roles }) => {
            handle_roles_available(roles, state, writer)
        }
        _ => Ok(()),
    }
}

fn write_wait_replies<W: Write>(
    replies: Vec<WaitReply>,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    for reply in replies {
        write_wait_reply(reply, writer)?;
    }
    Ok(())
}

fn write_wait_start<W: Write>(
    start: WaitStart,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    if let Some(call_id) = start.suppress_call_id {
        write_background_notification_suppress(call_id, writer)?;
    }
    if let Some(reply) = start.reply {
        write_wait_reply(reply, writer)?;
    }
    Ok(())
}

fn handle_ext_agent_query_result<W: Write>(
    result: tau_proto::ExtAgentQueryResult,
    state: &mut RunState,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    let Some(pending) = state.pending.remove(&result.query_id) else {
        tracing::warn!(
            target: LOG_TARGET,
            query_id = %result.query_id,
            "received result for unknown query_id",
        );
        return Ok(());
    };
    let duration_seconds = delegate_duration_seconds(pending.started_at.elapsed());
    let call_id = pending.call_id;
    let tool_name = pending.tool_name;

    if let Some(error) = result.error {
        tracing::debug!(
            target: LOG_TARGET,
            query_id = %result.query_id,
            error = %error,
            "delegation failed",
        );
        let tool_error = ToolError {
            call_id,
            tool_name,
            tool_type: tau_proto::ToolType::Function,
            message: error,
            details: delegate_error_details(duration_seconds),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        };
        write_wait_replies(
            state
                .wait_tracker
                .record_delegate_tool_error(tool_error.clone()),
            writer,
        )?;
        writer.write_frame(&Frame::Event(Event::ToolError(tool_error)))?;
    } else {
        tracing::debug!(
            target: LOG_TARGET,
            query_id = %result.query_id,
            text_len = result.text.len(),
            "delegation succeeded",
        );
        let tool_result = ToolResult {
            call_id,
            tool_name,
            tool_type: tau_proto::ToolType::Function,
            result: delegate_result_value(result.text, duration_seconds),
            kind: ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        };
        write_wait_replies(
            state
                .wait_tracker
                .record_delegate_tool_result(tool_result.clone()),
            writer,
        )?;
        writer.write_frame(&Frame::Event(Event::ToolResult(tool_result)))?;
    }
    writer.flush()?;
    Ok(())
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

fn handle_roles_available<W: Write>(
    roles: Vec<tau_proto::HarnessRoleInfo>,
    state: &mut RunState,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    state.latest_roles = roles
        .into_iter()
        .map(|role| {
            serde_json::json!({
                "name": role.name,
                "description": role.role_description.unwrap_or(role.description),
            })
        })
        .collect();
    state
        .latest_roles
        .sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    if let Some(session_id) = state.current_session_id.clone() {
        publish_delegate_roles_context(writer, session_id, &state.latest_roles)?;
    }
    Ok(())
}

fn handle_tool_invoke<W: Write>(
    invoke: ToolInvoke,
    pending: &mut HashMap<String, PendingDelegate>,
    next_query_id: &mut u64,
    wait_tracker: &mut WaitTracker,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    match invoke.tool_name.as_str() {
        WAIT_TOOL_NAME => {
            let start = wait_tracker.handle_wait_invoke(invoke);
            write_wait_start(start, writer)?;
            return Ok(());
        }
        DELEGATE_TOOL_NAME => wait_tracker.record_tool_invoke(&invoke),
        _ => {
            wait_tracker.record_tool_invoke(&invoke);
            return Ok(());
        }
    }

    let parsed = match parse_args(&invoke.arguments) {
        Ok(parsed) => parsed,
        Err(message) => {
            tracing::debug!(
                target: LOG_TARGET,
                error = %message,
                "rejecting delegate invocation: bad arguments",
            );
            writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
                call_id: invoke.call_id,
                tool_name: invoke.tool_name,
                tool_type: tau_proto::ToolType::Function,
                message,
                details: Some(invoke.arguments),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            })))?;
            writer.flush()?;
            return Ok(());
        }
    };

    let query_id = format!("delegate-{next_query_id}");
    *next_query_id += 1;
    let call_id = invoke.call_id.clone();
    tracing::info!(
        target: LOG_TARGET,
        query_id = %query_id,
        task_name = %parsed.task_name,
        execution_mode = ?parsed.execution_mode,
        prompt_len = parsed.prompt.len(),
        role = ?parsed.role,
        "dispatching delegation",
    );
    pending.insert(
        query_id.clone(),
        PendingDelegate {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
            started_at: Instant::now(),
        },
    );
    writer.write_frame(&Frame::Event(Event::ExtAgentQuery(ExtAgentQuery {
        query_id,
        instruction: format!("{DELEGATE_PREFIX}{}", parsed.prompt),
        role: parsed.role,
        execution_mode: parsed.execution_mode,
        input_stats: ToolDisplayStats::for_text(&parsed.prompt),
        // Hand the parent call_id and the agent-supplied task name to
        // the harness so it can route sub-agent progress
        // (`DelegateProgress`) under this tool block and the CLI can
        // render `delegate [name] …`.
        tool_call_id: Some(call_id),
        task_name: Some(parsed.task_name),
    })))?;
    writer.flush()?;
    Ok(())
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
    fn record_tool_invoke(&mut self, invoke: &ToolInvoke) {
        self.calls
            .entry(invoke.call_id.clone())
            .or_insert(WaitCallState::Pending);
    }

    fn handle_wait_invoke(&mut self, invoke: ToolInvoke) -> WaitStart {
        let target = match parse_wait_args(&invoke.arguments) {
            Ok(target) => target,
            Err(message) => {
                return WaitStart::reply(wait_error_reply(
                    invoke.call_id,
                    invoke.tool_name,
                    message,
                    Some(invoke.arguments),
                ));
            }
        };
        let wait = WaitRequest {
            call_id: invoke.call_id,
            tool_name: invoke.tool_name,
        };
        self.start_wait(target, wait)
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
        self.calls
            .insert(call_id.clone(), WaitCallState::NormalReturned);
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
        self.calls
            .insert(call_id.clone(), WaitCallState::NormalReturned);
        Vec::new()
    }

    fn record_delegate_tool_result(&mut self, result: ToolResult) -> Vec<WaitReply> {
        if self.is_backgrounded(&result.call_id) {
            return self.record_background_result(ToolBackgroundResult {
                call_id: result.call_id,
                tool_name: result.tool_name,
                tool_type: result.tool_type,
                result: result.result,
                display: result.display,
                originator: result.originator,
            });
        }
        self.record_tool_result(result)
    }

    fn record_delegate_tool_error(&mut self, error: ToolError) -> Vec<WaitReply> {
        if self.is_backgrounded(&error.call_id) {
            return self.record_background_error(ToolBackgroundError {
                call_id: error.call_id,
                tool_name: error.tool_name,
                tool_type: error.tool_type,
                message: error.message,
                details: error.details,
                display: error.display,
                originator: error.originator,
            });
        }
        self.record_tool_error(error)
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
    }
}

fn write_wait_reply<W: Write>(
    reply: WaitReply,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    if let Some(call_id) = reply.suppress_call_id.clone() {
        write_background_notification_suppress(call_id, writer)?;
    }
    match reply.kind {
        WaitReplyKind::Result { result, display } => {
            writer.write_frame(&Frame::Event(Event::ToolResult(ToolResult {
                call_id: reply.wait_call_id,
                tool_name: reply.wait_tool_name,
                tool_type: tau_proto::ToolType::Function,
                result,
                kind: ToolResultKind::Final,
                display,
                originator: tau_proto::PromptOriginator::User,
            })))?;
        }
        WaitReplyKind::Error {
            message,
            details,
            display,
        } => {
            writer.write_frame(&Frame::Event(Event::ToolError(ToolError {
                call_id: reply.wait_call_id,
                tool_name: reply.wait_tool_name,
                tool_type: tau_proto::ToolType::Function,
                message,
                details,
                display,
                originator: tau_proto::PromptOriginator::User,
            })))?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn write_background_notification_suppress<W: Write>(
    call_id: ToolCallId,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Event(Event::ToolBackgroundNotificationSuppress(
        ToolBackgroundNotificationSuppress { call_id },
    )))?;
    writer.flush()?;
    Ok(())
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

fn ack_log_event<W: Write>(
    id: LogEventId,
    writer: &mut FrameWriter<BufWriter<W>>,
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Message(Message::Ack(Ack { up_to: id })))?;
    writer.flush()?;
    Ok(())
}

fn publish_delegate_roles_context<W: Write>(
    writer: &mut FrameWriter<BufWriter<W>>,
    session_id: tau_proto::SessionId,
    roles: &[serde_json::Value],
) -> Result<(), Box<dyn Error>> {
    writer.write_frame(&Frame::Event(Event::ExtSessionContextPublish(
        ExtSessionContextPublish {
            session_id,
            key: SessionContextKey::new("delegate_roles"),
            value: SessionContextValue(serde_json::Value::Array(roles.to_vec())),
        },
    )))?;
    writer.flush()?;
    Ok(())
}

fn publish_delegate_roles_context_and_ready<W: Write>(
    writer: &mut FrameWriter<BufWriter<W>>,
    session_id: tau_proto::SessionId,
    roles: &[serde_json::Value],
) -> Result<(), Box<dyn Error>> {
    // The harness waits for every supervised tool extension subscribed to
    // `session.started` to report that its per-session context is complete.
    // Delegate subscribes only to publish the prompt-visible role list, so the
    // readiness signal belongs immediately after that publish.
    publish_delegate_roles_context(writer, session_id.clone(), roles)?;
    writer.write_frame(&Frame::Event(Event::ExtensionContextReady(
        ExtensionContextReady { session_id },
    )))?;
    writer.flush()?;
    Ok(())
}

fn delegate_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(DELEGATE_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Delegate a self-contained sub-task to a fresh sub-agent that runs with its own context and tools, and returns only its final text answer. Use it for: open-ended exploration where step count is unpredictable; large search/read sweeps whose intermediate output would otherwise clutter this conversation; parallel work — multiple delegations with `execution_mode: \"shared\"` can overlap globally. Use `execution_mode: \"exclusive\"` when the sub-agent needs to run alone: it waits for all other sub-agent delegations and blocks later independent ones until it finishes. Skip it when the target is already known (use direct tools like `read`/`grep`/`shell` instead) or when the task requires synthesis you should do yourself — don't push 'based on findings, fix the bug' onto a sub-agent; investigate first, then delegate the concrete change. The sub-agent starts with a *clean* conversation: it sees ONLY your `prompt`, plus its tools and system prompt. It cannot see this conversation's prior turns, your reasoning, files you've read, or earlier tool results — and that isolation applies at every nesting depth, so a sub-agent's own delegations are equally fresh. You must therefore brief the sub-agent fully: state the goal, hand it every fact it needs (absolute file paths, exact symbols, code snippets, prior findings, constraints, format of the answer you want), and frame the sub-task as if writing to a teammate who just walked into the room. Terse command-style prompts produce shallow, generic work; missing context produces wrong answers."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Short human-readable label for the sub-task (a few words, lowercase). Surfaced live to the user as `delegate [task_name]` while the sub-agent runs."
                },
                "prompt": {
                    "type": "string",
                    "description": "Self-contained task for the sub-agent. The sub-agent's conversation starts fresh — it has NO access to this conversation's history, your earlier tool results, or files you've read. State everything it needs: the goal, the relevant facts (absolute file paths, exact symbols, snippets you've already extracted), any constraints, what counts as 'done', and the format of the answer you want back. Treat it like briefing a teammate who just walked into the room. Terse command-style prompts produce shallow work; missing context produces wrong answers."
                },
                "execution_mode": {
                    "type": "string",
                    "enum": ["shared", "exclusive"],
                    "description": "Use `shared` when the sub-task can safely overlap globally with other shared sub-agent delegations. Use `exclusive` when it must run alone: it waits for all other sub-agent delegations and blocks later independent ones. Default: `shared`."
                },
                "role": {
                    "type": "string",
                    "description": "Optional sub-agent role to use. When omitted, Tau defaults delegate calls to `engineer` if that role is available and enabled."
                }
            },
            "required": ["task_name", "prompt"]
        })),
        format: None,
        enabled_by_default: true,
        // The delegate tool itself can dispatch alongside other shared
        // tools in the parent conversation; the `ExtAgentQuery` global
        // scheduler below enforces per-delegation exclusivity.
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Instant),
    }
}

fn wait_tool_spec() -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new(WAIT_TOOL_NAME),
        model_visible_name: None,
        description: Some(
            "Wait for a tool call by `tool_call_id` and return its completed result. Tau will notify you via marked internal messages about tool calls running in the background, and when they complete. Invoke only for calls you know are complete already."
                .to_owned(),
        ),
        tool_type: tau_proto::ToolType::Function,
        parameters: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "tool_call_id": {
                    "type": "string",
                    "description": "The tool call id to wait for."
                }
            },
            "required": ["tool_call_id"]
        })),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: Some(BackgroundSupport::Never),
    }
}

#[cfg_attr(test, derive(Debug))]
struct DelegateArgs {
    task_name: String,
    prompt: String,
    execution_mode: ToolExecutionMode,
    role: Option<String>,
}

fn parse_args(arguments: &CborValue) -> Result<DelegateArgs, String> {
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
            // Compatibility with the pre-`execution_mode` schema. Only true was
            // meaningful: it requested overlap for safe read-only work, which is
            // now represented as Shared. Explicit `execution_mode` wins because
            // omitted mode also defaults to Shared below.
            "read_only" => match v {
                CborValue::Bool(_) => {}
                _ => return Err("`read_only` must be a boolean".to_owned()),
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
    let role = role.filter(|role| !role.trim().is_empty());
    let execution_mode = execution_mode.unwrap_or(ToolExecutionMode::Shared);
    Ok(DelegateArgs {
        task_name,
        prompt,
        execution_mode,
        role,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(entries: &[(&str, CborValue)]) -> CborValue {
        CborValue::Map(
            entries
                .iter()
                .map(|(k, v)| (CborValue::Text((*k).to_owned()), v.clone()))
                .collect(),
        )
    }

    fn text(s: &str) -> CborValue {
        CborValue::Text(s.to_owned())
    }

    fn map_field<'a>(value: &'a CborValue, name: &str) -> Option<&'a CborValue> {
        let CborValue::Map(entries) = value else {
            return None;
        };
        entries.iter().find_map(|(key, value)| match key {
            CborValue::Text(key) if key == name => Some(value),
            _ => None,
        })
    }

    #[test]
    fn delegate_duration_seconds_matches_shell_semantics() {
        // Delegate results use the same slow-call header behavior as `shell`:
        // omit exact-threshold calls, and round slower calls up to whole seconds.
        assert_eq!(delegate_duration_seconds(Duration::from_secs(5)), None);
        assert_eq!(
            delegate_duration_seconds(Duration::from_millis(5_001)),
            Some(6)
        );
    }

    #[test]
    fn delegate_result_adds_duration_header_only_for_slow_calls() {
        // Keep fast delegate payloads as plain text for compatibility, but use
        // an `output` map when a slow-call `duration_seconds` header is needed.
        assert_eq!(
            delegate_result_value("answer".to_owned(), None),
            text("answer")
        );

        let value = delegate_result_value("answer".to_owned(), Some(6));
        assert_eq!(map_field(&value, "output"), Some(&text("answer")));
        assert_eq!(
            map_field(&value, "duration_seconds"),
            Some(&CborValue::Integer(6.into()))
        );
    }

    #[test]
    fn delegate_error_details_adds_duration_header_only_for_slow_calls() {
        // Slow failed delegate calls should expose the same duration header;
        // fast failures keep `details` empty for compatibility.
        assert_eq!(delegate_error_details(None), None);

        let details = delegate_error_details(Some(6)).expect("details");
        assert_eq!(map_field(&details, "output"), None);
        assert_eq!(
            map_field(&details, "duration_seconds"),
            Some(&CborValue::Integer(6.into()))
        );
    }

    fn wait_request(call_id: &str) -> WaitRequest {
        WaitRequest {
            call_id: call_id.into(),
            tool_name: ToolName::new(WAIT_TOOL_NAME),
        }
    }

    fn expect_wait_reply(start: WaitStart) -> WaitReply {
        start.reply.expect("wait should reply")
    }

    #[test]
    fn parse_wait_args_trims_tool_call_id() {
        // Model-provided ids can pick up incidental whitespace from copying.
        // Normalize it before matching against the exact recorded tool call id.
        assert_eq!(
            parse_wait_args(&args(&[("tool_call_id", text(" call-1 \n"))])),
            Ok(ToolCallId::from("call-1"))
        );
    }

    fn assert_wait_pending(start: WaitStart) {
        assert_eq!(start, WaitStart::default());
    }

    fn tool_result(call_id: &str, result: CborValue) -> ToolResult {
        ToolResult {
            call_id: call_id.into(),
            tool_name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result,
            kind: ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }
    }

    fn background_placeholder(call_id: &str) -> ToolResult {
        ToolResult {
            call_id: call_id.into(),
            tool_name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result: text("background placeholder"),
            kind: ToolResultKind::BackgroundPlaceholder,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }
    }

    fn background_result(call_id: &str, result: CborValue) -> ToolBackgroundResult {
        ToolBackgroundResult {
            call_id: call_id.into(),
            tool_name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            result,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }
    }

    fn background_error(call_id: &str, message: &str) -> ToolBackgroundError {
        ToolBackgroundError {
            call_id: call_id.into(),
            tool_name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            message: message.to_owned(),
            details: Some(text("details")),
            display: None,
            originator: tau_proto::PromptOriginator::User,
        }
    }

    #[test]
    fn parses_valid_args() {
        let parsed = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
        ]))
        .expect("valid args parse");
        assert_eq!(parsed.task_name, "audit");
        assert_eq!(parsed.prompt, "do the thing");
        assert_eq!(parsed.execution_mode, ToolExecutionMode::Shared);
        assert_eq!(parsed.role, None);
    }

    /// Delegate role is optional metadata passed through to the harness; empty
    /// strings are treated as omitted so older agents that hallucinate blank
    /// fields do not change behavior.
    #[test]
    fn parses_optional_role() {
        let parsed = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("role", text("rush")),
        ]))
        .expect("role args parse");
        assert_eq!(parsed.role.as_deref(), Some("rush"));

        let omitted = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("role", text("   ")),
        ]))
        .expect("blank role args parse");
        assert_eq!(omitted.role, None);
    }

    /// Regression coverage for global sub-agent scheduling intent: omitted
    /// execution_mode is shared, explicit exclusive is preserved, and legacy
    /// read_only:true remains accepted as shared for older agents.
    #[test]
    fn parses_execution_mode_and_legacy_read_only() {
        let exclusive = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("execution_mode", text("exclusive")),
            ("read_only", CborValue::Bool(true)),
        ]))
        .expect("explicit execution_mode parses");
        assert_eq!(exclusive.execution_mode, ToolExecutionMode::Exclusive);

        let legacy = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("read_only", CborValue::Bool(true)),
        ]))
        .expect("legacy read_only parses");
        assert_eq!(legacy.execution_mode, ToolExecutionMode::Shared);
    }

    /// Regression coverage for the agent-visible terminology: delegate should
    /// advertise shared/exclusive execution modes plus optional role selection,
    /// and keep the legacy alias out of the schema/description.
    #[test]
    fn tool_schema_advertises_execution_mode_and_role() {
        let spec = delegate_tool_spec();
        let description = spec.description.expect("description");
        assert!(description.contains("execution_mode"));
        assert!(description.contains("shared"));
        assert!(!description.contains("read_only"));
        assert!(!description.contains("read-only"));
        assert!(!description.contains("Mutating"));

        let parameters = spec.parameters.expect("parameters");
        let properties = parameters
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("object properties");
        assert!(properties.contains_key("execution_mode"));
        assert!(properties.contains_key("role"));
        assert!(!properties.contains_key("read_only"));
        assert_eq!(spec.execution_mode, ToolExecutionMode::Shared);
        assert_eq!(spec.background_support, Some(BackgroundSupport::Instant));
    }

    /// The wait tool must never background itself: it represents an explicit
    /// request to block the model turn until another call's terminal result is
    /// available.
    #[test]
    fn wait_tool_schema_disables_backgrounding() {
        let spec = wait_tool_spec();
        assert_eq!(spec.name.as_str(), WAIT_TOOL_NAME);
        assert_eq!(spec.background_support, Some(BackgroundSupport::Never));
        let description = spec.description.expect("description");
        assert!(description.contains("marked internal messages"));
        assert!(description.contains("Invoke only for calls you know are complete already"));
    }

    /// Unknown call ids fail immediately so the model can recover instead of
    /// leaving a wait invocation pending forever.
    #[test]
    fn wait_unknown_call_id_errors() {
        let mut tracker = WaitTracker::default();
        let start = tracker.start_wait("missing".into(), wait_request("wait-1"));
        let reply = start.reply.expect("unknown call replies immediately");
        assert!(matches!(
            reply.kind,
            WaitReplyKind::Error { message, .. } if message.contains("unknown tool call")
        ));
    }

    /// Foreground completions already went back to the model through the normal
    /// tool result path; wait returns a clear error instead of duplicating the
    /// original payload.
    #[test]
    fn wait_normal_foreground_returns_not_backgrounded_error_once() {
        let mut tracker = WaitTracker::default();
        tracker.record_tool_result(tool_result("call-1", text("done")));

        let first = tracker.start_wait("call-1".into(), wait_request("wait-1"));
        let first = first.reply.expect("completed call replies");
        assert!(matches!(
            first.kind,
            WaitReplyKind::Error { message, .. } if message.contains("returned normally, not backgrounded")
        ));
        let second = tracker.start_wait("call-1".into(), wait_request("wait-2"));
        let second = second.reply.expect("second wait errors");
        assert!(matches!(
            second.kind,
            WaitReplyKind::Error { message, .. } if message.contains("already consumed")
        ));
    }

    /// A background result is returned exactly once with its original CBOR
    /// payload so `wait` does not lose structured tool output.
    #[test]
    fn wait_background_result_once_preserves_payload() {
        let mut tracker = WaitTracker::default();
        tracker.record_background_result(background_result("call-1", text("real result")));

        let first = expect_wait_reply(tracker.start_wait("call-1".into(), wait_request("wait-1")));
        assert_eq!(
            first.kind,
            WaitReplyKind::Result {
                result: text("real result"),
                display: None,
            }
        );
        let second = expect_wait_reply(tracker.start_wait("call-1".into(), wait_request("wait-2")));
        assert!(matches!(second.kind, WaitReplyKind::Error { .. }));
    }

    /// Duplicate waits for the same still-pending target fail immediately so
    /// the extension never has two wait tool calls racing to consume one
    /// result.
    #[test]
    fn duplicate_wait_for_pending_call_errors() {
        let mut tracker = WaitTracker::default();
        tracker.record_tool_invoke(&ToolInvoke {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            arguments: args(&[]),
            originator: tau_proto::PromptOriginator::User,
        });
        assert_wait_pending(tracker.start_wait("call-1".into(), wait_request("wait-1")));

        let second = expect_wait_reply(tracker.start_wait("call-1".into(), wait_request("wait-2")));
        assert!(matches!(
            second.kind,
            WaitReplyKind::Error { message, .. }
                if message == "existing wait for this tool already in progress"
        ));
    }

    /// A synthetic foreground background placeholder keeps wait pending until
    /// the real background completion event arrives.
    #[test]
    fn wait_ignores_synthetic_background_placeholder() {
        let mut tracker = WaitTracker::default();
        tracker.record_tool_invoke(&ToolInvoke {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            arguments: args(&[]),
            originator: tau_proto::PromptOriginator::User,
        });
        assert!(
            tracker
                .record_tool_result(background_placeholder("call-1"))
                .is_empty()
        );
        assert_eq!(
            tracker.start_wait("call-1".into(), wait_request("wait-1")),
            WaitStart::suppress("call-1".into())
        );

        let replies = tracker.record_background_result(background_result("call-1", text("real")));
        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0].kind,
            WaitReplyKind::Result {
                result: text("real"),
                display: None,
            }
        );
    }

    /// Background errors are surfaced as wait tool errors and preserve both the
    /// error message and details payload from the original tool.
    #[test]
    fn wait_background_error_once_preserves_error() {
        let mut tracker = WaitTracker::default();
        tracker.record_background_error(background_error("call-1", "boom"));

        let first = expect_wait_reply(tracker.start_wait("call-1".into(), wait_request("wait-1")));
        assert_eq!(
            first.kind,
            WaitReplyKind::Error {
                message: "boom".to_owned(),
                details: Some(text("details")),
                display: None,
            }
        );
        let second = expect_wait_reply(tracker.start_wait("call-1".into(), wait_request("wait-2")));
        assert!(matches!(second.kind, WaitReplyKind::Error { .. }));
    }

    /// If a call is still foreground when wait starts, the wait invocation
    /// stays pending and is woken with the exact later normal tool result.
    #[test]
    fn wait_blocks_until_foreground_completion_wakes_waiter() {
        let mut tracker = WaitTracker::default();
        tracker.record_tool_invoke(&ToolInvoke {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            arguments: args(&[]),
            originator: tau_proto::PromptOriginator::User,
        });
        assert_wait_pending(tracker.start_wait("call-1".into(), wait_request("wait-1")));

        let replies = tracker.record_tool_result(tool_result("call-1", text("done")));
        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0].kind,
            WaitReplyKind::Result {
                result: text("done"),
                display: None,
            }
        );
    }

    /// A delegate result is emitted before the harness echoes the same result
    /// back to this extension. The echo must not reopen a consumed wait result.
    #[test]
    fn wait_consumed_delegate_result_ignores_normal_echo() {
        let mut tracker = WaitTracker::default();
        tracker.record_tool_invoke(&ToolInvoke {
            call_id: "call-1".into(),
            tool_name: ToolName::new("delegate"),
            arguments: args(&[]),
            originator: tau_proto::PromptOriginator::User,
        });
        assert_wait_pending(tracker.start_wait("call-1".into(), wait_request("wait-1")));

        let replies = tracker.record_delegate_tool_result(tool_result("call-1", text("done")));
        assert_eq!(replies.len(), 1);
        assert!(
            tracker
                .record_tool_result(tool_result("call-1", text("done")))
                .is_empty()
        );
        let second = expect_wait_reply(tracker.start_wait("call-1".into(), wait_request("wait-2")));
        assert!(matches!(second.kind, WaitReplyKind::Error { .. }));
    }

    /// Background delegate completions also echo through the event log. That
    /// echo must not overwrite the consumed state and permit a second wait.
    #[test]
    fn wait_consumed_delegate_background_result_ignores_background_echo() {
        let mut tracker = WaitTracker::default();
        tracker.record_tool_invoke(&ToolInvoke {
            call_id: "call-1".into(),
            tool_name: ToolName::new("delegate"),
            arguments: args(&[]),
            originator: tau_proto::PromptOriginator::User,
        });
        assert!(
            tracker
                .record_tool_result(background_placeholder("call-1"))
                .is_empty()
        );
        assert_eq!(
            tracker.start_wait("call-1".into(), wait_request("wait-1")),
            WaitStart::suppress("call-1".into())
        );

        let replies = tracker.record_delegate_tool_result(tool_result("call-1", text("real")));
        assert_eq!(replies.len(), 1);
        assert!(
            tracker
                .record_background_result(background_result("call-1", text("real")))
                .is_empty()
        );
        let second = expect_wait_reply(tracker.start_wait("call-1".into(), wait_request("wait-2")));
        assert!(matches!(second.kind, WaitReplyKind::Error { .. }));
    }

    /// Delegate progress should show the size of the user-provided prompt,
    /// not the private prefix inserted before the sub-agent sees it.
    #[test]
    fn tool_invoke_reports_input_stats_for_user_prompt_only() {
        let prompt = "first\nsecond";
        let mut pending = HashMap::new();
        let mut next_query_id = 0;
        let mut wait_tracker = WaitTracker::default();
        let mut output = Vec::new();
        {
            let mut writer = FrameWriter::new(BufWriter::new(&mut output));
            handle_tool_invoke(
                ToolInvoke {
                    call_id: "call-1".into(),
                    tool_name: tau_proto::ToolName::new(TOOL_NAME),
                    arguments: args(&[("task_name", text("audit")), ("prompt", text(prompt))]),
                    originator: tau_proto::PromptOriginator::User,
                },
                &mut pending,
                &mut next_query_id,
                &mut wait_tracker,
                &mut writer,
            )
            .expect("delegate invocation handled");
        }

        let mut reader = FrameReader::new(BufReader::new(output.as_slice()));
        let frame = reader
            .read_frame()
            .expect("read query frame")
            .expect("query frame present");
        let Frame::Event(Event::ExtAgentQuery(query)) = frame else {
            panic!("expected ExtAgentQuery, got {frame:?}");
        };
        assert!(query.instruction.starts_with(DELEGATE_PREFIX));
        assert!(query.instruction.ends_with(prompt));
        assert_eq!(query.input_stats, ToolDisplayStats::for_text(prompt));
        assert_ne!(
            query.input_stats,
            ToolDisplayStats::for_text(&query.instruction)
        );
    }

    /// Regression coverage for the 9c3088c "don't special case foreman"
    /// change: subscribing to `session.started` makes the harness wait for this
    /// extension during session init, so delegate must pair its context publish
    /// with `extension.context_ready`.
    #[test]
    fn session_started_context_publish_is_followed_by_context_ready() {
        let mut output = Vec::new();
        {
            let mut writer = FrameWriter::new(BufWriter::new(&mut output));
            publish_delegate_roles_context_and_ready(
                &mut writer,
                "s1".into(),
                &[serde_json::json!({
                    "name": "deep",
                    "description": "Deep research",
                })],
            )
            .expect("publish context and ready");
        }

        let mut reader = FrameReader::new(BufReader::new(output.as_slice()));
        let first = reader
            .read_frame()
            .expect("read context frame")
            .expect("context frame present");
        assert!(matches!(
            first,
            Frame::Event(Event::ExtSessionContextPublish(publish))
                if publish.session_id.as_str() == "s1"
                    && publish.key.as_str() == "delegate_roles"
        ));

        let second = reader
            .read_frame()
            .expect("read ready frame")
            .expect("ready frame present");
        assert!(matches!(
            second,
            Frame::Event(Event::ExtensionContextReady(ready))
                if ready.session_id.as_str() == "s1"
        ));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let parsed = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("read_only", CborValue::Bool(true)),
            ("future_field", CborValue::Integer(7.into())),
        ]))
        .expect("unknown keys ignored");
        assert_eq!(parsed.task_name, "audit");
    }

    #[test]
    fn rejects_non_map_arguments() {
        let err = parse_args(&CborValue::Text("nope".to_owned()))
            .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("arguments must be an object"), "got: {err}");
    }

    #[test]
    fn rejects_missing_prompt() {
        let err = parse_args(&args(&[("task_name", text("audit"))]))
            .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("prompt"), "got: {err}");
    }

    #[test]
    fn rejects_missing_task_name() {
        let err = parse_args(&args(&[("prompt", text("do the thing"))]))
            .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("task_name"), "got: {err}");
    }

    #[test]
    fn rejects_empty_prompt() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("   \n")),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`prompt` must not be empty"), "got: {err}");
    }

    #[test]
    fn rejects_empty_task_name() {
        let err = parse_args(&args(&[
            ("task_name", text("")),
            ("prompt", text("do the thing")),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`task_name` must not be empty"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_prompt() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", CborValue::Integer(42.into())),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`prompt` must be a string"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_task_name() {
        let err = parse_args(&args(&[
            ("task_name", CborValue::Bool(false)),
            ("prompt", text("do the thing")),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`task_name` must be a string"), "got: {err}");
    }

    #[test]
    fn rejects_wrong_type_for_role() {
        let err = parse_args(&args(&[
            ("task_name", text("audit")),
            ("prompt", text("do the thing")),
            ("role", CborValue::Bool(false)),
        ]))
        .expect_err("parse_args should reject invalid arguments");
        assert!(err.contains("`role` must be a string"), "got: {err}");
    }
}
