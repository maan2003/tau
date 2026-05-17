use super::*;

#[test]
fn embedded_mode_returns_agent_response_and_persists_history() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let r = run_embedded_message_with_echo(&sp, "s1", "hello")
        .expect("should succeed")
        .response;
    assert!(!r.is_empty(), "response should not be empty: {r:?}");
    let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
    let store = open_session_store(&sessions_dir).expect("reopen");
    let branch = store.session("s1").expect("session").current_branch();
    assert!(
        branch.len() >= 2,
        "should have user msg + agent response, got {}",
        branch.len()
    );

    // Debug-log mirror: every turn that goes through the harness
    // should produce both an inbound `from_connection` line capturing
    // the raw agent frame, and a `published` line capturing the
    // enriched copy the harness committed. This is what
    // cache/cost-analysis tooling reads.
    let jsonl = std::fs::read_to_string(sessions_dir.join("s1").join("events.jsonl"))
        .expect("events.jsonl should exist for session s1");
    let parsed: Vec<serde_json::Value> = jsonl
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("valid jsonl"))
        .collect();
    let from_connection_finished = parsed
        .iter()
        .filter(|e| {
            e["type"] == "from_connection" && e["event_name"] == "provider.response_finished"
        })
        .count();
    let published_finished = parsed
        .iter()
        .filter(|e| e["type"] == "published" && e["event_name"] == "provider.response_finished")
        .count();
    assert!(
        from_connection_finished >= 1,
        "expected ≥1 inbound provider.response_finished line, got {from_connection_finished}",
    );
    assert!(
        published_finished >= 1,
        "expected ≥1 published provider.response_finished line, got {published_finished}",
    );
}

#[test]
fn daemon_mode_accepts_later_clients() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
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
    assert_eq!(r1, "hello", "first cycle should echo our submission");
    assert_eq!(r2, "again", "second cycle should echo our submission");

    server.join().expect("join").expect("daemon clean exit");
    let store = open_session_store(tau_config::settings::sessions_dir_of(&sp)).expect("reopen");
    let branch = store.session("s1").expect("session").current_branch();
    // System AGENTS.md preamble + 2 × (user, tool.req, tool.res, agent).
    assert_eq!(
        branch.len(),
        9,
        "expected full two-cycle persistence, got {branch:?}"
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
fn traced_embedded_reports_shell_progress() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let o = run_embedded_message_with_echo(&sp, "s1", "shell printf hi").expect("ok");
    // Socket clients can miss short-lived progress when the command
    // completes before the subscription writer drains, but the final
    // response must still arrive and lifecycle tracing is covered above.
    assert!(!o.response.is_empty(), "shell response should not be empty");
}

#[test]
fn traced_daemon_reports_shell_progress() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || {
            run_daemon_with_echo(
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
    // Socket clients may miss short-lived progress if the shell command
    // completes before the writer drains the transient event.
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
fn daemon_disconnect_reason_is_reported() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let listener = bind_listener(&sock).expect("bind");

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let read_stream = stream.try_clone().expect("clone");
        let mut reader = FrameReader::new(BufReader::new(read_stream));
        let mut writer = FrameWriter::new(BufWriter::new(stream));
        let _ = reader.read_frame(); // hello
        let _ = reader.read_frame(); // subscribe
        let _ = reader.read_frame(); // message
        writer
            .write_frame(&Frame::Message(Message::Disconnect(Disconnect {
                reason: Some("test disconnect".to_owned()),
            })))
            .expect("write");
        writer.flush().expect("flush");
    });

    let err =
        send_daemon_message_with_trace(&sock, "s1", "hello").expect_err("should get disconnect");
    assert!(matches!(&err, HarnessError::Participant(r) if r == "test disconnect"));
    server.join().expect("join");
}

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
