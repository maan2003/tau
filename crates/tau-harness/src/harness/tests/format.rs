use super::*;

#[test]
fn format_agent_entry_tree_preview_shows_grouped_tool_results() {
    let result = AgentEntry::ToolResults {
        items: vec![tau_proto::ToolResultItem {
            call_id: "call_ugly".into(),
            tool_type: tau_proto::ToolType::Function,
            status: tau_proto::ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("hello".to_owned())),
        }],
    };
    assert_eq!(
        format_agent_entry(&result),
        "tool.result call_ugly -> hello"
    );

    let multibyte_result = AgentEntry::ToolResults {
        items: vec![tau_proto::ToolResultItem {
            call_id: "call_utf8".into(),
            tool_type: tau_proto::ToolType::Function,
            status: tau_proto::ToolResultStatus::Success,
            output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("é".repeat(81))),
        }],
    };
    let formatted = format_agent_entry(&multibyte_result);
    assert!(formatted.ends_with("..."));
}

#[test]
fn policy_lines_are_printable() {
    let td = TempDir::new().expect("tempdir");
    let sock = td.path().join("daemon.sock");
    let sp = td.path().join("state");

    let server = thread::spawn({
        let sock = sock.clone();
        let sp = sp.clone();
        move || run_daemon_with_echo(sock, sp, ServeOptions::builder().max_clients(1).build())
    });

    let started = Instant::now();
    while !sock.exists() {
        assert!(started.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(10));
    }

    let _ = send_daemon_message_with_trace(&sock, "hello").expect("ok");
    server.join().expect("join").expect("clean exit");

    let pl = policy_lines(sp.join("policy.cbor")).expect("policy");
    assert!(pl.iter().any(|l| l.contains("socket-ui")));
}

#[test]
fn empty_policy_view() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    assert_eq!(
        policy_lines(sp.join("policy.cbor")).expect("ok"),
        vec!["no policy approvals"]
    );
}
