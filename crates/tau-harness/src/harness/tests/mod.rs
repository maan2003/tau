//! Test suite for the harness. Split by concern to mirror the
//! production module layout (interception, replay, skill_tool, dispatch, …).
//!
//! The shared helpers and imports live here so each submodule can
//! pull them in with `use super::*;`.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tau_core::{
    Connection, ConnectionMetadata, ConnectionOrigin, ConnectionSendError, ConnectionSink,
    RoutedFrame, SessionEntry,
};
use tau_proto::{
    AgentResponseFinished, AgentResponseUpdated, CborValue, ContentPart, ContextItem, ContextRole,
    Disconnect, Event, EventSelector, ExtAgentQuery, Frame, FrameReader, FrameWriter, Intercept,
    InterceptAction, InterceptReply, InterceptionPriority, Message, MessageItem,
    SessionCompactionRequested, SessionPromptCreated, SessionPromptId, SessionPromptQueued,
    Subscribe, ToolCallId, ToolCallItem, ToolName, ToolResult, ToolResultItem, ToolResultStatus,
    ToolSideEffects, ToolSpec, UiPromptDraft, UiPromptSubmitted,
};
use tau_session_inspect::{
    default_session_id, format_session_entry, open_session_store, policy_lines, session_lines,
    session_list_lines,
};
use tempfile::TempDir;

use super::{AgentToolCall, HARNESS_CONNECTION_ID, Harness};
use crate::conversation::ConversationTurnState;
use crate::daemon::{
    ServeOptions, bind_listener, run_daemon_with_echo, run_embedded_message_with_echo,
    send_daemon_message, send_daemon_message_with_trace,
};
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::model::{
    baseline_params_for_selection, clamp_effort, efforts_for_model, load_roles,
    select_model_for_available, selected_params_for_role, thinking_summaries_for_model,
    verbosities_for_model,
};
use crate::prompt::build_system_prompt;
use crate::turn::{PromptSubmission, TurnState};

fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    crate::harness::run_echo_agent(r, w).map_err(|e| e.to_string())
}

/// Test-only helper that pushes a `UiPromptSubmitted` through the
/// harness's normal publish path, which writes the durable per-session
/// event and folds it into the SessionTree. Production code reaches
/// the same place via `dispatch_user_prompt`; tests use this when
/// they want a tree node without driving the full agent turn.
fn append_user_message_via_event(h: &mut Harness, session_id: &str, text: &str) {
    h.publish_event(
        None,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: session_id.into(),
            text: text.to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
}

fn echo_harness(state_dir: impl Into<PathBuf>) -> Result<Harness, HarnessError> {
    echo_harness_for("s1", state_dir)
}

fn echo_harness_for(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
) -> Result<Harness, HarnessError> {
    let state_dir = state_dir.into();
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(state_dir.join("config")),
        state_dir: Some(state_dir.join("runtime")),
    };
    echo_harness_with_dirs(session_id, state_dir, dirs)
}

fn echo_harness_with_dirs(
    session_id: &str,
    state_dir: impl Into<PathBuf>,
    dirs: tau_config::settings::TauDirs,
) -> Result<Harness, HarnessError> {
    fn shell_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
        tau_ext_shell::run(r, w).map_err(|e| e.to_string())
    }
    Harness::new_with_agent(
        state_dir,
        dirs,
        echo_runner,
        vec![crate::harness::InProcessTool {
            name: "shell",
            runner: shell_runner,
        }],
        session_id,
    )
}

struct TestSink {
    events: Arc<Mutex<Vec<RoutedFrame>>>,
}

impl ConnectionSink for TestSink {
    fn send(&mut self, event: RoutedFrame) -> Result<(), ConnectionSendError> {
        self.events.lock().expect("sink mutex").push(event);
        Ok(())
    }
}

fn connect_test_client(
    h: &mut Harness,
    name: &str,
    kind: tau_proto::ClientKind,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    h.bus.connect(Connection::new(
        ConnectionMetadata {
            id: name.into(),
            name: name.to_owned(),
            kind,
            origin: ConnectionOrigin::InMemory,
        },
        Box::new(TestSink {
            events: Arc::clone(&events),
        }),
    ));
    events
}

fn connect_test_tool(h: &mut Harness, name: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    connect_test_client(h, name, tau_proto::ClientKind::Tool)
}

/// Pre-seed the per-conversation `AgentThinking` state for tests that
/// bypass `dispatch_prompt_for_conversation` and call response handlers
/// directly.
fn seed_agent_thinking(h: &mut Harness, cid: &crate::conversation::ConversationId, spid: &str) {
    h.conversations
        .get_mut(cid)
        .expect("conversation present")
        .turn_state = ConversationTurnState::AgentThinking {
        session_prompt_id: spid.into(),
    };
}

/// Pre-seed the per-conversation `ToolsRunning` state for tests that
/// bypass the agent-response path and call tool handlers directly.
fn seed_tools_running(
    h: &mut Harness,
    cid: &crate::conversation::ConversationId,
    remaining: Vec<ToolCallId>,
) {
    h.conversations
        .get_mut(cid)
        .expect("conversation present")
        .turn_state = ConversationTurnState::ToolsRunning {
        remaining_calls: remaining,
    };
}

/// Seed the transcript and turn state as if the assistant had just
/// emitted one or more tool calls for this conversation.
fn seed_assistant_tool_round(
    h: &mut Harness,
    cid: &crate::conversation::ConversationId,
    calls: &[(&str, &str)],
) {
    h.publish_for_conversation(
        cid,
        Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-seeded-tools".into(),
            output_items: calls
                .iter()
                .map(|(call_id, tool_name)| {
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: (*call_id).into(),
                        name: ToolName::new(*tool_name),
                        tool_type: tau_proto::ToolType::Function,
                        arguments: CborValue::Map(Vec::new()),
                    })
                })
                .collect(),
            stop_reason: tau_proto::AgentStopReason::ToolCalls,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    seed_tools_running(
        h,
        cid,
        calls.iter().map(|(call_id, _)| (*call_id).into()).collect(),
    );
}

/// Pumps the harness event loop until the named tool call's result
/// or error is received and handled. Panics on timeout.
fn drive_harness_until_call_completes(h: &mut Harness, target_call_id: &str) {
    let started = Instant::now();
    loop {
        if started.elapsed() >= Duration::from_secs(3) {
            panic!("timed out waiting for {target_call_id} to complete");
        }
        let event =
            h.rx.recv_timeout(Duration::from_secs(1))
                .expect("tool result should arrive");
        match event {
            HarnessEvent::FromConnection {
                connection_id,
                frame,
            } => {
                let is_target = match frame.as_ref() {
                    Frame::Event(Event::ToolResult(r)) => r.call_id.as_str() == target_call_id,
                    Frame::Event(Event::ToolError(e)) => e.call_id.as_str() == target_call_id,
                    _ => false,
                };
                h.handle_extension_event(&connection_id, *frame)
                    .expect("handle");
                if is_target {
                    return;
                }
            }
            HarnessEvent::Disconnected { connection_id } => {
                h.handle_disconnect(&connection_id);
            }
            HarnessEvent::NewClient(_) => {}
        }
    }
}

fn wait_for_session_unlock(state_dir: &Path, session_id: &str) {
    let sessions_dir = tau_config::settings::sessions_dir_of(state_dir);
    let started = Instant::now();
    loop {
        let locked =
            tau_core::session_is_locked(&sessions_dir, session_id).expect("session lock probe");
        if !locked {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timed out waiting for session `{session_id}` lock to clear"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Find the conversation id of the outer side conversation (the one
/// whose originator is the delegate extension's first query). Used by
/// the cross-conversation regression test above to disambiguate
/// nested-vs-outer side prompt ids.
fn outer_side_cid_str(h: &Harness) -> &str {
    h.conversations
        .iter()
        .find_map(|(cid, conv)| {
            matches!(
                &conv.originator,
                tau_proto::PromptOriginator::Extension { query_id, .. }
                    if query_id == "q-outer"
            )
            .then_some(cid.as_str())
        })
        .unwrap_or("")
}

/// Subscribe a fresh test sink to `tool.delegate_progress` events and
/// hand back its accumulator.
fn collect_event_sink(h: &mut Harness) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let events = connect_test_tool(h, "test-delegate-progress-sink");
    h.bus
        .set_subscriptions(
            "test-delegate-progress-sink",
            vec![tau_proto::EventSelector::Exact(
                tau_proto::EventName::TOOL_DELEGATE_PROGRESS,
            )],
        )
        .expect("subscribe");
    events
}

/// Peel a routed frame to its bus-event payload, unwrapping the
/// `Message::LogEvent` envelope when present. Returns `None` for
/// non-event messages (Hello, Ack, …).
fn peel_inner_event(frame: &Frame) -> Option<&Event> {
    match frame {
        Frame::Event(event) => Some(event),
        Frame::Message(Message::LogEvent(env)) => Some(&env.event),
        Frame::Message(_) => None,
    }
}

fn pop_delegate_progress(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    call_id: &str,
) -> Option<tau_proto::DelegateProgress> {
    let mut events = sink.lock().expect("sink");
    let pos = events.iter().position(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolDelegateProgress(p)) if p.call_id.as_str() == call_id
        )
    })?;
    let removed = events.remove(pos);
    match removed.frame {
        Frame::Event(Event::ToolDelegateProgress(p)) => Some(p),
        Frame::Message(Message::LogEvent(env)) => match *env.event {
            Event::ToolDelegateProgress(p) => Some(p),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

fn drain_delegate_progress(
    sink: &Arc<Mutex<Vec<RoutedFrame>>>,
    call_id: &str,
) -> Vec<tau_proto::DelegateProgress> {
    let mut events = sink.lock().expect("sink");
    let mut out = Vec::new();
    events.retain(|routed| match peel_inner_event(&routed.frame) {
        Some(Event::ToolDelegateProgress(p)) if p.call_id.as_str() == call_id => {
            out.push(p.clone());
            false
        }
        _ => true,
    });
    out
}

fn read_raw_prompt_created(h: &Harness, spid: &SessionPromptId) -> SessionPromptCreated {
    let mut cursor = 0;
    loop {
        let entry = h
            .event_log
            .get_next_from(cursor)
            .expect("prompt event in log");
        cursor = entry.seq + 1;
        match entry.event {
            Event::SessionPromptCreated(prompt) if &prompt.session_prompt_id == spid => {
                return prompt;
            }
            _ => {}
        }
    }
}

fn read_raw_compaction_requested(
    h: &Harness,
    spid: &SessionPromptId,
) -> SessionCompactionRequested {
    let mut cursor = 0;
    loop {
        let entry = h
            .event_log
            .get_next_from(cursor)
            .expect("compaction request event in log");
        cursor = entry.seq + 1;
        match entry.event {
            Event::SessionCompactionRequested(request)
                if &request.prompt.session_prompt_id == spid =>
            {
                return request;
            }
            _ => {}
        }
    }
}

fn read_prompt_created(h: &Harness, spid: &SessionPromptId) -> SessionPromptCreated {
    h.read_session_prompt_created(spid)
        .expect("materialized prompt event")
}

fn read_compaction_requested(h: &Harness, spid: &SessionPromptId) -> SessionPromptCreated {
    let request = read_raw_compaction_requested(h, spid);
    h.materialize_session_prompt_created(&request.prompt)
        .expect("materialized compaction request")
}

fn intercepted_payload(events: &Arc<Mutex<Vec<RoutedFrame>>>) -> (Event, bool) {
    let events = events.lock().expect("events mutex");
    let intercepted = events
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Message(Message::InterceptRequest(req)) => Some(req),
            _ => None,
        })
        .expect("intercept request delivered");
    ((*intercepted.event).clone(), intercepted.transient)
}

fn draft_event(text: &str) -> Event {
    Event::UiPromptDraft(UiPromptDraft {
        session_id: "s1".into(),
        text: text.to_owned(),
    })
}

#[test]
fn shell_command_args_middle_shortens_long_first_line() {
    assert_eq!(
        super::shell_command_args(
            "printf 1234567890123456789012345678901234567890\nprintf ignored"
        ),
        "printf 1234567890123┄12345678901234567890"
    );
}

#[test]
fn shell_command_args_keeps_short_first_line() {
    assert_eq!(
        super::shell_command_args("printf 1234567890123"),
        "printf 1234567890123"
    );
}

mod dedup;
mod dispatch;
mod format;
mod interception;
mod lifecycle;
mod mode;
mod model;
mod replay;
mod skill_tool;
