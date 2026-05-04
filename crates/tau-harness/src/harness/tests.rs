use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use tau_core::{SessionEntry, ToolActivityOutcome, ToolActivityRecord};
use tau_proto::{
    AgentResponseFinished, AgentToolCall, CborValue, Event, EventReader, EventSelector,
    EventWriter, LifecycleDisconnect, LifecycleSubscribe, SessionPromptCreated, SessionPromptId,
    ToolCallId, ToolName, ToolResult, UiPromptSubmitted,
};
use tempfile::TempDir;

use super::{Harness, default_agent_runner};
use crate::daemon::{
    ServeOptions, bind_listener, run_daemon, run_embedded_message_with_echo, send_daemon_message,
    send_daemon_message_with_trace,
};
use crate::dirs::{
    default_session_id, open_session_store, policy_lines, session_lines, session_list_lines,
};
use crate::discovery::{DiscoveredAgentsFile, DiscoveredSkill};
use crate::error::HarnessError;
use crate::event::HarnessEvent;
use crate::format::format_session_entry;
use crate::model::selected_effort_for_model;
use crate::prompt::build_system_prompt;
use crate::turn::{PromptSubmission, TurnState};

fn echo_runner(r: UnixStream, w: UnixStream) -> Result<(), String> {
    tau_agent::run_echo(r, w).map_err(|e| e.to_string())
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
    Harness::new_with_agent(
        state_dir,
        tau_config::settings::TauDirs::default(),
        echo_runner,
        true,
        session_id,
    )
}

#[test]
fn format_session_entry_tree_preview_hides_call_id_and_shows_skill_name() {
    let skill_request = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_HC8dStLuLeEjHCxFZsBx6jfV".into(),
        tool_name: "skill".into(),
        outcome: ToolActivityOutcome::Requested {
            arguments: CborValue::Map(vec![(
                CborValue::Text("name".to_owned()),
                CborValue::Text("jujutsu".to_owned()),
            )]),
        },
    });
    assert_eq!(
        format_session_entry(&skill_request),
        "tool.request skill jujutsu"
    );

    let read_request = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_ugly".into(),
        tool_name: "read".into(),
        outcome: ToolActivityOutcome::Requested {
            arguments: CborValue::Map(vec![(
                CborValue::Text("path".to_owned()),
                CborValue::Text("foo.txt".to_owned()),
            )]),
        },
    });
    assert_eq!(format_session_entry(&read_request), "tool.request read");

    let result = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_ugly".into(),
        tool_name: "read".into(),
        outcome: ToolActivityOutcome::Result {
            result: CborValue::Text("hello".to_owned()),
        },
    });
    assert_eq!(format_session_entry(&result), "tool.result read -> hello");
}

#[test]
fn embedded_mode_returns_agent_response_and_persists_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "hello")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "response should not be empty: {r:?}");
    let store = open_session_store(&sp).expect("reopen");
    let branch = store.session("s1").expect("session").current_branch();
    assert!(
        branch.len() >= 2,
        "should have user msg + agent response, got {}",
        branch.len()
    );
}

#[test]
#[ignore = "needs echo agent wired into run_daemon"]
fn daemon_mode_accepts_later_clients() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(2).build(),
            )
        }
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3), "socket timeout");
        thread::sleep(Duration::from_millis(10));
    }

    let r1 = send_daemon_message(&sock, "s1", "hello").expect("first");
    let r2 = send_daemon_message(&sock, "s1", "again").expect("second");
    assert!(!r1.is_empty(), "response should not be empty");
    assert!(!r2.is_empty(), "response should not be empty");

    server.join().expect("join").expect("daemon clean exit");
    let store = open_session_store(&sp).expect("reopen");
    assert_eq!(
        store.session("s1").expect("session").current_branch().len(),
        8
    );
}

#[test]
fn embedded_mode_can_read_files() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let fp = td.path().join("note.txt");
    std::fs::write(&fp, "hello from disk").expect("write fixture");
    let r = run_embedded_message_with_echo(&sp, "s1", &format!("read {}", fp.display()))
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "read response should not be empty");
    assert!(r.contains("hello from disk"));
}

#[test]
fn embedded_mode_can_run_shell_commands() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "shell printf hi")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "shell response should not be empty");
}

#[test]
fn unavailable_tool_is_reported_without_crashing() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let removed = h.registry.unregister_connection(&conn_id);
    assert!(removed.iter().any(|t| t == "shell"));

    let outcome = h
        .send_user_message("s1", "shell printf hi", None)
        .expect("should succeed with error");
    assert!(outcome.response.contains("no live provider available"));
    h.shutdown().expect("shutdown");
}

#[test]
fn disconnected_tool_completes_pending_call() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let call_id: ToolCallId = "call-1".into();
    let tool_name: ToolName = "shell".into();
    let cid = h.default_conversation_id.clone();
    h.tool_conversations.insert(call_id.clone(), cid);
    h.pending_tool_names
        .insert(call_id.clone(), tool_name.clone());
    h.pending_tool_providers
        .insert(call_id.clone(), conn_id.clone().into());
    h.in_flight_tool_kinds
        .insert(call_id.clone(), tau_proto::ToolSideEffects::Mutating);
    let cid_for_state = h.default_conversation_id.clone();
    h.turn_state = TurnState::ToolsRunning {
        session_id: "s1".into(),
        conversation_id: cid_for_state,
        remaining_calls: vec![call_id.clone()],
    };

    h.handle_disconnect(&conn_id);

    assert!(!matches!(h.turn_state, TurnState::ToolsRunning { .. }));
    assert!(!h.tool_conversations.contains_key(&call_id));
    assert!(!h.pending_tool_providers.contains_key(&call_id));

    let branch = h.store.session("s1").expect("session").current_branch();
    assert!(branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                call_id: logged_call_id,
                outcome: ToolActivityOutcome::Error { message, .. },
                ..
            }) if logged_call_id == &call_id && message == "tool provider disconnected"
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnected_tool_is_removed_cleanly() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Send disconnect to the extension via the bus (through the
    // writer channel → writer thread → stream).
    let _ = h.bus.send_to(
        &conn_id,
        None,
        Event::LifecycleDisconnect(LifecycleDisconnect {
            reason: Some("test".to_owned()),
        }),
    );

    // Drive event loop until the disconnect arrives.
    let started = Instant::now();
    loop {
        let event =
            h.rx.recv_timeout(Duration::from_secs(2))
                .expect("should get disconnect");
        match event {
            HarnessEvent::Disconnected {
                ref connection_id, ..
            } if *connection_id == conn_id => {
                h.handle_disconnect(&conn_id);
                break;
            }
            HarnessEvent::FromConnection {
                connection_id,
                event,
            } => {
                let _ = h.handle_extension_event(&connection_id, event);
            }
            _ => {}
        }
        assert!(started.elapsed() < Duration::from_secs(2), "timeout");
    }

    assert!(h.bus.connection(&conn_id).is_none());
    assert!(h.registry.providers_for("shell").is_empty());
    assert!(
        h.lifecycle_messages
            .iter()
            .any(|m| m == "extension shell exited")
    );

    let outcome = h
        .send_user_message("s1", "shell printf hi", None)
        .expect("should succeed with error");
    assert!(outcome.response.contains("no live provider available"));
    h.shutdown().expect("shutdown");
}

#[test]
fn traced_embedded_reports_shell_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "shell printf hi").expect("ok");
    assert_eq!(o.progress_messages, vec!["shell: running shell command"]);
    assert!(!o.response.is_empty(), "shell response should not be empty");
}

#[test]
#[ignore = "needs echo agent wired into run_daemon"]
fn traced_daemon_reports_shell_progress() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }

    let o = send_daemon_message_with_trace(&sock, "s1", "shell printf hi").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension shell ready")
    );
    assert_eq!(o.progress_messages, vec!["shell: running shell command"]);
    assert!(!o.response.is_empty(), "shell response should not be empty");
    server.join().expect("join").expect("clean exit");
}

#[test]
fn traced_embedded_reports_lifecycle() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "hello").expect("ok");
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent starting")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent ready")
    );
    assert!(
        o.lifecycle_messages
            .iter()
            .any(|m| m == "extension agent exited")
    );
}

#[test]
#[ignore = "needs echo agent wired into run_daemon"]
fn session_and_policy_lines_are_printable() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon(
                sock,
                sp,
                "s1",
                ServeOptions::builder().max_clients(1).build(),
            )
        }
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }

    let _ = send_daemon_message_with_trace(&sock, "s1", "hello").expect("ok");
    server.join().expect("join").expect("clean exit");

    let sl = session_lines(&sp, "s1").expect("lines");
    assert!(sl.iter().any(|l| l.contains("user: hello")));
    assert!(sl.iter().any(|l| l.contains("tool.request echo")));
    let sll = session_list_lines(&sp).expect("list");
    assert!(sll.iter().any(|l| l.contains("s1 (4 entries)")));
    let pl = policy_lines(&sp.join("policy.cbor")).expect("policy");
    assert!(pl.iter().any(|l| l.contains("socket-ui")));
}

#[test]
fn empty_session_and_policy_views() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    std::fs::create_dir_all(&sp).expect("mkdir");
    assert_eq!(session_list_lines(&sp).expect("ok"), vec!["no sessions"]);
    assert_eq!(
        policy_lines(&sp.join("policy.cbor")).expect("ok"),
        vec!["no policy approvals"]
    );
    assert_eq!(
        session_lines(&sp, "x").expect("ok"),
        vec!["session x not found"]
    );
}

#[test]
fn daemon_disconnect_reason_is_reported() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let listener = bind_listener(&sock).expect("bind");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let read_stream = stream.try_clone().expect("clone");
        let mut reader = EventReader::new(BufReader::new(read_stream));
        let mut writer = EventWriter::new(BufWriter::new(stream));
        let _ = reader.read_event(); // hello
        let _ = reader.read_event(); // subscribe
        let _ = reader.read_event(); // message
        writer
            .write_event(&Event::LifecycleDisconnect(LifecycleDisconnect {
                reason: Some("test disconnect".to_owned()),
            }))
            .expect("write");
        writer.flush().expect("flush");
    });

    let err =
        send_daemon_message_with_trace(&sock, "s1", "hello").expect_err("should get disconnect");
    assert!(matches!(&err, HarnessError::Participant(r) if r == "test disconnect"));
    server.join().expect("join");
}

// -- AGENTS.md --

#[test]
fn agents_context_is_injected_at_session_init() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_connection_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Eager init at construction may have already appended a real
    // AGENTS.md (ext-shell walks the test cwd). Clear so we assert
    // only on the test-injected pair below.
    h.discovered_agents_files.clear();
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/AGENTS.md"),
        content: "# Root\n- root rule\n".to_owned(),
    });
    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/pkg/AGENTS.md"),
        content: "# Package\n- package rule\n".to_owned(),
    });
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        waiting_on: [tools_connection_id.clone().into()].into_iter().collect(),
    };
    h.handle_extension_event(
        &tools_connection_id,
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            session_id: "s1".into(),
        }),
    )
    .expect("ready");

    assert!(matches!(h.turn_state, TurnState::Idle));

    let branch = h.store.session("s1").expect("session").current_branch();
    let injected = branch
        .iter()
        .rev()
        .find_map(|e| match e {
            SessionEntry::UserMessage { text }
                if text.starts_with("# AGENTS.md instructions")
                    && text.contains("/repo/AGENTS.md") =>
            {
                Some(text.as_str())
            }
            _ => None,
        })
        .expect("expected injected AGENTS.md user message");
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/AGENTS.md\">"));
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/pkg/AGENTS.md\">"));
    let root_pos = injected.find("root rule").expect("root rule");
    let pkg_pos = injected.find("package rule").expect("package rule");
    assert!(
        root_pos < pkg_pos,
        "broader file should appear before nested one"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn cross_session_prompt_is_rejected() {
    // The harness owns one session at a time. A UserMessage with
    // a different session id must not silently spin up a second
    // session — it gets rejected with a clear reason.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start"); // bound to "s1"

    h.selected_model = "test/model".into();
    let submission = h
        .submit_user_prompt("chat-1".into(), "hello".to_owned())
        .expect("submit");
    match submission {
        PromptSubmission::Rejected { reason } => {
            assert!(reason.contains("s1"), "reason should name bound session");
            assert!(reason.contains("chat-1"), "reason should name rejected id");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
    assert!(
        h.pending_prompts.is_empty(),
        "rejected prompt must not queue"
    );
    assert!(
        h.store.session("chat-1").is_none(),
        "rejected session must not be created"
    );

    h.shutdown().expect("shutdown");
}

// -- Eager session init --

#[test]
fn harness_startup_eagerly_initializes_eager_session() {
    // Guards against the recurring "this looks like redundant work"
    // urge to lazy-ify session init. `echo_harness` calls
    // `Harness::new_with_agent`, which must eagerly initialize the
    // session before returning — see the design-choice comment in
    // the constructor for why.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let h = echo_harness(&sp).expect("start");

    assert!(
        h.initialized_sessions.contains("s1"),
        "eager init should mark the bound session as initialized at startup; \
         `initialized_sessions` was {:?}",
        h.initialized_sessions
    );
    assert!(
        matches!(h.turn_state, TurnState::Idle),
        "turn state should be Idle after eager init completes"
    );
}

#[test]
fn late_joining_ui_client_receives_replayed_session_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.send_user_message("s1", "hello replay", None)
        .expect("send message");

    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events
            .iter()
            .any(|entry| matches!(entry.event, Event::UiPromptSubmitted(_))),
        "user prompt should be in durable session event log"
    );
    assert!(
        events
            .iter()
            .any(|entry| matches!(entry.event, Event::AgentResponseFinished(_))),
        "final agent response should be in durable session event log"
    );
    assert!(
        events.iter().all(|entry| !entry.event.is_transient()),
        "transient events must not be persisted"
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    h.handle_client_event(
        &ui_conn,
        Event::LifecycleSubscribe(LifecycleSubscribe {
            selectors: vec![
                EventSelector::Prefix("ui.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
            ],
        }),
    )
    .expect("subscribe");

    let mut reader = EventReader::new(BufReader::new(client_end));
    let mut got_prompt = false;
    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !(got_prompt && got_response) {
        let Ok(Some(event)) = reader.read_event() else {
            break;
        };
        let (_log_id, inner) = event.peel_log();
        match inner {
            Event::UiPromptSubmitted(prompt) if prompt.text == "hello replay" => {
                got_prompt = true;
            }
            Event::AgentResponseFinished(finished)
                if finished
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("hello replay")) =>
            {
                got_response = true;
            }
            _ => {}
        }
    }

    assert!(got_prompt, "late UI should replay prior user prompt");
    assert!(got_response, "late UI should replay prior agent response");

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_receives_replayed_agents_md_and_context_ready() {
    // The CLI connects after the daemon's eager init has already
    // fired, so live subscription would miss `ExtAgentsMdAvailable`
    // and `ExtensionContextReady`. `replay_harness_info` must
    // replay them from the event log at subscribe time so the UI
    // still renders the "loaded: …" / "session context ready"
    // lines.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_conn = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Inject synthetic discovery events as if ext-shell had reported
    // them during eager init. publish_event appends to the log,
    // which is what `replay_harness_info` walks.
    h.publish_event(
        Some(&tools_conn),
        Event::ExtAgentsMdAvailable(tau_proto::ExtAgentsMdAvailable {
            file_path: "/test/AGENTS.md".into(),
            content: "# test\n".to_owned(),
        }),
    );
    h.publish_event(
        Some(&tools_conn),
        Event::ExtensionContextReady(tau_proto::ExtensionContextReady {
            session_id: default_session_id().into(),
        }),
    );

    // Hook up a fake UI client via a UnixStream pair.
    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("read timeout");
    h.accept_client(server_end).expect("accept");

    // Find the UI connection the bus assigned. `accept_client`
    // gives it name "socket-ui".
    let ui_conn = h
        .bus
        .connections()
        .into_iter()
        .find(|c| c.name == "socket-ui")
        .expect("ui connection")
        .id
        .to_string();

    // Trigger subscribe + replay via the normal client-event path.
    h.handle_client_event(
        &ui_conn,
        Event::LifecycleSubscribe(LifecycleSubscribe {
            selectors: vec![EventSelector::Prefix("extension.".to_owned())],
        }),
    )
    .expect("subscribe");

    // Read from the client side and collect the replayed discovery
    // events. Other `extension.*` events (starting/ready for fs +
    // agent extensions) also replay — we ignore them.
    let mut reader = EventReader::new(BufReader::new(client_end));
    let mut got_agents_md = false;
    let mut got_context_ready = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !(got_agents_md && got_context_ready) {
        let Ok(Some(event)) = reader.read_event() else {
            break;
        };
        let (_log_id, inner) = event.peel_log();
        match inner {
            Event::ExtAgentsMdAvailable(a)
                if a.file_path == std::path::Path::new("/test/AGENTS.md") =>
            {
                got_agents_md = true;
            }
            Event::ExtensionContextReady(_) => {
                got_context_ready = true;
            }
            _ => {}
        }
    }
    assert!(
        got_agents_md,
        "late UI client should replay ExtAgentsMdAvailable"
    );
    assert!(
        got_context_ready,
        "late UI client should replay ExtensionContextReady"
    );

    h.shutdown().expect("shutdown");
}

// -- Invalid tool call rejection --

#[test]
fn empty_tool_name_does_not_panic_and_surfaces_error() {
    // Agents occasionally emit tool_calls with empty names
    // (hallucinations, streaming-token splits, model bugs).
    // `ToolName::new("")` panics by design, so the harness must
    // reject these cleanly before that construction happens.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed as if the agent had just been prompted and is now
    // responding with tool_calls.
    h.selected_model = "test/model".into();
    let cid = h.default_conversation_id.clone();
    h.turn_state = TurnState::AgentThinking {
        _session_id: "s1".into(),
        conversation_id: cid.clone(),
    };
    h.prompt_conversations.insert("sp-x".into(), cid);

    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![AgentToolCall {
            id: "c1".into(),
            // Intentionally an empty raw string to exercise the
            // `Invalid` arm of `ToolNameMaybe`.
            name: "".into(),
            arguments: CborValue::Map(Vec::new()),
        }],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    };

    h.handle_agent_response_finished(response)
        .expect("invalid tool call must not panic");

    // The call must be gone from both the pending queue and the
    // in-flight set — rejection fully completes it.
    assert!(h.pending_tool_invocations.is_empty());
    assert!(h.in_flight_tool_kinds.is_empty());

    // The error should have been persisted on s1's history so the
    // agent sees it on the next turn — as a Requested + Error pair
    // under the same call_id, so the Responses-API serializer can
    // emit a matching `function_call` / `function_call_output`
    // without the latter looking unpaired.
    let branch = h.store.session("s1").expect("session").current_branch();
    let mut saw_request = false;
    let mut saw_error = false;
    for entry in branch.iter() {
        let SessionEntry::ToolActivity(record) = entry else {
            continue;
        };
        if record.call_id.as_str() != "c1" {
            continue;
        }
        match &record.outcome {
            ToolActivityOutcome::Requested { .. } => saw_request = true,
            ToolActivityOutcome::Error { message, .. } if message.contains("invalid tool name") => {
                saw_error = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_request && saw_error,
        "rejected call should leave both a Requested and an Error \
         ToolActivity so the model-facing conversation has a \
         matching tool_use / tool_result pair"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn empty_tool_call_id_is_normalized_to_synthetic_id() {
    // Models that hallucinate an invalid tool_call often drop the
    // `call_id` too. An empty id breaks two things downstream:
    // it collides with itself as a HashMap key, and it renders
    // into the next prompt as `input[N].call_id: ""` which the
    // OpenAI Responses API rejects outright. Normalize at the
    // boundary.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = "test/model".into();
    let cid = h.default_conversation_id.clone();
    h.turn_state = TurnState::AgentThinking {
        _session_id: "s1".into(),
        conversation_id: cid.clone(),
    };
    h.prompt_conversations.insert("sp-x".into(), cid);

    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "".into(),
                name: "".into(),
                arguments: CborValue::Map(Vec::new()),
            },
            AgentToolCall {
                id: "".into(),
                name: "".into(),
                arguments: CborValue::Map(Vec::new()),
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    };

    h.handle_agent_response_finished(response)
        .expect("must not panic");

    // Both calls were rejected and the turn is fully drained.
    assert!(h.pending_tool_invocations.is_empty());
    assert!(h.in_flight_tool_kinds.is_empty());

    // Every persisted ToolActivityRecord must have a non-empty
    // call_id — this is what the LLM serializer round-trips.
    // And each rejected call must appear TWICE (a Requested +
    // Error pair) so the model-facing conversation has a
    // matching function_call for the function_call_output.
    let branch = h.store.session("s1").expect("session").current_branch();
    let activity_records: Vec<_> = branch
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::ToolActivity(record) => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(
        activity_records.len(),
        4,
        "expected two records per rejected call (Requested + Error)"
    );
    let mut synth_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in &activity_records {
        assert!(
            !record.call_id.as_str().is_empty(),
            "synthesized call_id must not be empty; got {:?}",
            record.call_id
        );
        assert!(
            record.call_id.as_str().starts_with("harness-synth-"),
            "synthesized call_id should be clearly synthetic; got {:?}",
            record.call_id
        );
        synth_ids.insert(record.call_id.as_str().to_owned());
    }
    // Exactly two distinct synthetic ids across the four records.
    assert_eq!(
        synth_ids.len(),
        2,
        "the two rejected calls must have distinct synthetic ids; got {synth_ids:?}"
    );

    h.shutdown().expect("shutdown");
}

// -- Tool dispatch state machine --

#[test]
fn pure_mutating_pure_serializes_through_dispatch_state_machine() {
    use tau_proto::ToolSideEffects::{Mutating, Pure};

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed turn state as if the agent had just been prompted
    // and is about to respond with tool calls.
    h.selected_model = "test/model".into();
    let cid = h.default_conversation_id.clone();
    h.turn_state = TurnState::AgentThinking {
        _session_id: "s1".into(),
        conversation_id: cid.clone(),
    };
    h.prompt_conversations.insert("sp-x".into(), cid);

    // A `read` of a nonexistent path returns a ToolError (Pure);
    // `write` of a valid path creates the file and returns
    // ToolResult (Mutating). Either kind of response path is
    // handled identically by the state machine.
    let read_args = CborValue::Map(vec![(
        CborValue::Text("path".to_owned()),
        CborValue::Text("/nonexistent/tau-test-path".to_owned()),
    )]);
    let write_args = CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text(td.path().join("w.txt").display().to_string()),
        ),
        (
            CborValue::Text("content".to_owned()),
            CborValue::Text("hi".to_owned()),
        ),
    ]);
    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        text: None,
        tool_calls: vec![
            AgentToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: read_args.clone(),
            },
            AgentToolCall {
                id: "c2".into(),
                name: "write".into(),
                arguments: write_args,
            },
            AgentToolCall {
                id: "c3".into(),
                name: "read".into(),
                arguments: read_args,
            },
        ],
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    };

    h.handle_agent_response_finished(response)
        .expect("finished");

    // Right after dispatch, only c1 (Pure) should be in-flight;
    // c2 (Mutating) and c3 (Pure behind the Mutating) must wait.
    let c1_id: ToolCallId = "c1".to_owned().into();
    let c2_id: ToolCallId = "c2".to_owned().into();
    let c3_id: ToolCallId = "c3".to_owned().into();
    assert_eq!(h.in_flight_tool_kinds.len(), 1);
    assert_eq!(h.in_flight_tool_kinds.get(&c1_id), Some(&Pure));
    assert_eq!(h.pending_tool_invocations.len(), 2);
    assert_eq!(h.pending_tool_invocations[0].1.id, "c2");
    assert_eq!(h.pending_tool_invocations[1].1.id, "c3");

    drive_harness_until_call_completes(&mut h, "c1");

    // After c1 completes the Mutating gate opens and c2 dispatches.
    // c3 must stay queued behind it.
    assert_eq!(h.in_flight_tool_kinds.len(), 1);
    assert_eq!(h.in_flight_tool_kinds.get(&c2_id), Some(&Mutating));
    assert_eq!(h.pending_tool_invocations.len(), 1);
    assert_eq!(h.pending_tool_invocations[0].1.id, "c3");

    drive_harness_until_call_completes(&mut h, "c2");

    // With the Mutating cleared, c3 finally dispatches.
    assert_eq!(h.in_flight_tool_kinds.len(), 1);
    assert_eq!(h.in_flight_tool_kinds.get(&c3_id), Some(&Pure));
    assert!(h.pending_tool_invocations.is_empty());

    drive_harness_until_call_completes(&mut h, "c3");
    assert!(h.in_flight_tool_kinds.is_empty());

    h.shutdown().expect("shutdown");
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
                event,
            } => {
                let is_target = match &event {
                    Event::ToolResult(r) => r.call_id.as_str() == target_call_id,
                    Event::ToolError(e) => e.call_id.as_str() == target_call_id,
                    _ => false,
                };
                h.handle_extension_event(&connection_id, event)
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

// -- At-least-once delivery --

#[test]
fn extension_ack_advances_cursor() {
    // Verifies the at-least-once cursor: after the harness receives
    // an Ack from an extension, that extension's `last_acked` field
    // reflects the highest acked id.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    h.handle_extension_event(
        &tools_id,
        Event::Ack(tau_proto::Ack {
            up_to: tau_proto::LogEventId::new(7),
        }),
    )
    .expect("ack");

    let tools = h
        .extensions
        .iter()
        .find(|e| e.connection_id.as_str() == tools_id)
        .expect("entry");
    assert_eq!(tools.last_acked, tau_proto::LogEventId::new(7));
    h.shutdown().expect("shutdown");
}

#[test]
fn duplicate_ack_is_ignored() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let before = h
        .extensions
        .iter()
        .find(|e| e.connection_id.as_str() == tools_id)
        .expect("entry")
        .last_acked;

    // Resending an old ack must not move the cursor backward and
    // must not bump it forward either.
    h.handle_extension_event(
        &tools_id,
        Event::Ack(tau_proto::Ack {
            up_to: tau_proto::LogEventId::new(0),
        }),
    )
    .expect("ack");

    let after = h
        .extensions
        .iter()
        .find(|e| e.connection_id.as_str() == tools_id)
        .expect("entry")
        .last_acked;
    assert_eq!(before, after, "stale ack should not change cursor");
    h.shutdown().expect("shutdown");
}

// -- Skills --

#[test]
fn selected_effort_is_model_specific_and_clamped() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    std::fs::write(
        config_dir.join("harness.json5"),
        r#"{
            default_efforts: {
                "openai/gpt-4.1": "high",
                "local/llama": "high",
            },
        }"#,
    )
    .expect("write harness config");
    std::fs::write(
        config_dir.join("models.json5"),
        r#"{
            providers: {
                local: {
                    compat: { supportsReasoningEffort: false },
                    models: [{ id: "llama" }],
                },
                openai: {
                    compat: { supportsReasoningEffort: true },
                    models: [{ id: "gpt-4.1" }],
                },
            },
        }"#,
    )
    .expect("write models");
    std::fs::write(
        state_dir.join("harness.json5"),
        r#"{
            "last_selected_model": "openai/gpt-4.1",
            "last_efforts": {
                "openai/gpt-4.1": "minimal",
                "local/llama": "high"
            }
        }"#,
    )
    .expect("write state");

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let model_registry = tau_config::settings::load_models_in(&dirs).expect("load models");

    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "openai/gpt-4.1",),
        tau_proto::Effort::High
    );
    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "local/llama"),
        tau_proto::Effort::Off
    );
}

/// First-time users (no per-model entry in `default_efforts`, no
/// persisted `last_efforts`) get the middle of the available
/// reasoning levels, not the lowest. For the standard
/// reasoning-supporting list (`[Off, Minimal, Low, Medium, High]`)
/// that's `Low`. Non-reasoning providers stay at `Off`.
#[test]
fn fresh_install_picks_middle_effort_when_no_history() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir.clone()),
        state_dir: Some(state_dir.clone()),
    };

    // No harness.json5: default settings, empty default_efforts.
    std::fs::write(
        config_dir.join("models.json5"),
        r#"{
            providers: {
                local: {
                    compat: { supportsReasoningEffort: false },
                    models: [{ id: "llama" }],
                },
                openai: {
                    compat: { supportsReasoningEffort: true },
                    models: [{ id: "gpt-4.1" }],
                },
            },
        }"#,
    )
    .expect("write models");
    // No harness.json5: fresh install.

    let harness_settings =
        tau_config::settings::load_harness_settings_in(&dirs).expect("load harness settings");
    let model_registry = tau_config::settings::load_models_in(&dirs).expect("load models");

    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "openai/gpt-4.1"),
        tau_proto::Effort::Low,
    );
    assert_eq!(
        selected_effort_for_model(&dirs, &harness_settings, &model_registry, "local/llama"),
        tau_proto::Effort::Off,
    );
}

#[test]
fn build_system_prompt_includes_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("brave-search"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Web search via Brave API".to_owned(),
            file_path: PathBuf::from("/skills/brave-search/SKILL.md"),
            add_to_prompt: true,
        },
    );
    let prompt = build_system_prompt(&[], &skills, "/tmp/work");
    assert!(prompt.contains("<available_skills>"));
    assert!(prompt.contains("<name>brave-search</name>"));
    assert!(prompt.contains("Web search via Brave API"));
    assert!(!prompt.contains("Current date:"));
    assert!(prompt.contains("Current working directory: /tmp/work"));
}

#[test]
fn build_system_prompt_excludes_hidden_skills() {
    let mut skills = std::collections::HashMap::new();
    skills.insert(
        tau_proto::SkillName::from("hidden"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "Should not appear".to_owned(),
            file_path: PathBuf::from("/skills/hidden/SKILL.md"),
            add_to_prompt: false,
        },
    );
    let prompt = build_system_prompt(&[], &skills, "/tmp/work");
    assert!(!prompt.contains("<available_skills>"));
    assert!(!prompt.contains("hidden"));
}

#[test]
fn linear_session_prompts_strictly_extend_previous_messages() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = "test/model".into();

    append_user_message_via_event(&mut h, "s1", "hello");

    let spid1 = h.send_prompt_to_agent("s1");
    let prompt1 = read_prompt_created(&h, &spid1);

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("hi".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: None,
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("persist first agent response");

    append_user_message_via_event(&mut h, "s1", "again");

    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);

    assert_eq!(prompt2.system_prompt, prompt1.system_prompt);
    assert_eq!(prompt2.tools, prompt1.tools);
    assert_eq!(prompt2.model, prompt1.model);
    assert_eq!(prompt2.effort, prompt1.effort);
    assert!(
        prompt1.messages.len() < prompt2.messages.len(),
        "second prompt should strictly extend first: {} !< {}",
        prompt1.messages.len(),
        prompt2.messages.len()
    );
    assert_eq!(
        &prompt2.messages[..prompt1.messages.len()],
        prompt1.messages.as_slice(),
        "second prompt must keep first prompt messages as an exact prefix"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn thinking_is_persisted_but_excluded_from_prompt_replay() {
    // Linear-prefix and prompt-cache hygiene depends on
    // `assemble_conversation` ignoring the persisted thinking
    // field. Otherwise the model would see its own reasoning
    // summary echoed back as plain assistant text.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = "test/model".into();

    append_user_message_via_event(&mut h, "s1", "first");

    let spid1 = h.send_prompt_to_agent("s1");
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: spid1,
        text: Some("answer".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        thinking: Some("The user is asking ...".to_owned()),
        originator: tau_proto::PromptOriginator::User,
    })
    .expect("persist agent response");

    // Confirm it was stored on the session entry.
    let stored = h
        .store
        .session("s1")
        .expect("session")
        .current_branch()
        .into_iter()
        .find_map(|e| match e {
            SessionEntry::AgentMessage { thinking, .. } => Some(thinking.clone()),
            _ => None,
        })
        .expect("agent message");
    assert_eq!(stored.as_deref(), Some("The user is asking ..."));

    // The next prompt's replayed messages must NOT contain the
    // thinking text.
    append_user_message_via_event(&mut h, "s1", "second");
    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);
    let serialized = serde_json::to_string(&prompt2.messages).expect("json");
    assert!(
        !serialized.contains("The user is asking"),
        "prompt replay must not echo reasoning summary back to the model",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn skill_tool_reads_file_content() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let skill_dir = td.path().join("my-skill");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        "---\nname: my-skill\ndescription: A test skill\n---\n# Instructions\nDo the thing.",
    )
    .expect("write");

    let mut h = echo_harness(&sp).expect("start");

    // Manually insert a discovered skill.
    h.discovered_skills.insert(
        tau_proto::SkillName::from("my-skill"),
        DiscoveredSkill {
            source_id: "skills".into(),
            description: "A test skill".to_owned(),
            file_path: skill_file,
            add_to_prompt: true,
        },
    );

    // Directly invoke the skill tool handler.
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    h.turn_state = TurnState::ToolsRunning {
        session_id: "s1".into(),
        conversation_id: cid_for_state,
        remaining_calls: vec!["call-skill".into()],
    };
    let call = AgentToolCall {
        id: "call-skill".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![(
            CborValue::Text("name".to_owned()),
            CborValue::Text("my-skill".to_owned()),
        )]),
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify the tool result was persisted.
    let branch = h.store.session("s1").expect("session").current_branch();
    let has_skill_result = branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                outcome: ToolActivityOutcome::Result { .. },
                ..
            })
        )
    });
    assert!(has_skill_result, "expected skill tool result in session");
    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolResult(result) if result.call_id.as_str() == "call-skill"
        )),
        "expected skill tool result in durable session event log"
    );
}

#[test]
fn skill_tool_returns_error_for_unknown_skill() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");
    append_user_message_via_event(&mut h, "s1", "load skill");
    let cid_for_state = h.default_conversation_id.clone();
    h.turn_state = TurnState::ToolsRunning {
        session_id: "s1".into(),
        conversation_id: cid_for_state,
        remaining_calls: vec!["call-missing".into()],
    };
    let call = AgentToolCall {
        id: "call-missing".into(),
        name: "skill".into(),
        arguments: CborValue::Map(vec![(
            CborValue::Text("name".to_owned()),
            CborValue::Text("nonexistent".to_owned()),
        )]),
    };
    let cid = h.default_conversation_id.clone();
    h.handle_skill_tool_call(&cid, &call).expect("skill call");

    // Verify a tool error was persisted.
    let branch = h.store.session("s1").expect("session").current_branch();
    let has_skill_error = branch.iter().any(|entry| {
        matches!(
            entry,
            SessionEntry::ToolActivity(ToolActivityRecord {
                outcome: ToolActivityOutcome::Error { .. },
                ..
            })
        )
    });
    assert!(has_skill_error, "expected skill tool error in session");
    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().any(|entry| matches!(
            &entry.event,
            Event::ToolError(error) if error.call_id.as_str() == "call-missing"
        )),
        "expected skill tool error in durable session event log"
    );
}

#[test]
fn skill_tool_registered_in_tool_list() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let h = echo_harness(&sp).expect("start");
    let defs = h.gather_tool_definitions();
    assert!(
        defs.iter().any(|d| d.name == "skill"),
        "skill tool should be registered; got: {:?}",
        defs.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
}

#[test]
fn duplicate_tool_result_is_discarded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");

    // Fabricate a tool result for a call_id that is not in pending_tool_sessions.
    let result = h.handle_extension_event(
        "fake-ext",
        Event::ToolResult(ToolResult {
            call_id: "orphan-call".into(),
            tool_name: "read".into(),
            result: tau_proto::CborValue::Text("stale data".to_owned()),
        }),
    );
    // Should not error — just emits a warning and discards.
    assert!(result.is_ok());
}

/// One-shot dump of the system prompt + first user turn the agent
/// receives, written to `tmp/initial_prompt.txt` at the repo root.
/// Uses the user's real `TauDirs::default()` config so cwd, skills
/// discovered by the shell extension, and the actual tool list
/// match what a real session would see (extensions defined in
/// `harness.json5` are not spawned by the embedded path, so only
/// shell-registered tools appear).
///
/// Run with:
///   cargo test -p tau-harness dump_initial_prompt_to_tmp -- --ignored
/// --nocapture
#[test]
#[ignore = "writes tmp/initial_prompt.txt; run with --ignored"]
fn dump_initial_prompt_to_tmp() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = Harness::new_with_agent(
        &sp,
        tau_config::settings::TauDirs::default(),
        default_agent_runner,
        false,
        "s1",
    )
    .expect("start harness");
    h.selected_model = "test/model".into();

    append_user_message_via_event(&mut h, "s1", "hello");

    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);

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
    out.push_str(&format!("effort: {:?}\n\n", prompt.effort));

    out.push_str("================ SYSTEM PROMPT ================\n");
    out.push_str(&prompt.system_prompt);
    if !prompt.system_prompt.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');

    out.push_str("================ MESSAGES ================\n");
    out.push_str(&serde_json::to_string_pretty(&prompt.messages).expect("messages json"));
    out.push_str("\n\n");

    out.push_str("================ TOOLS ================\n");
    out.push_str(&serde_json::to_string_pretty(&prompt.tools).expect("tools json"));
    out.push('\n');

    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let dest = repo_root.join("tmp").join("initial_prompt.txt");
    std::fs::create_dir_all(dest.parent().unwrap()).expect("create tmp/");
    std::fs::write(&dest, &out).expect("write dump");
    eprintln!("wrote {}", dest.display());

    h.shutdown().expect("shutdown");
}

fn read_prompt_created(h: &Harness, spid: &SessionPromptId) -> SessionPromptCreated {
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
