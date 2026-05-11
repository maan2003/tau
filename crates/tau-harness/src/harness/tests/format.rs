use super::*;

#[test]
fn format_session_entry_tree_preview_hides_call_id_and_shows_skill_name() {
    let skill_request = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_HC8dStLuLeEjHCxFZsBx6jfV".into(),
        tool_name: "skill".into(),
        outcome: ToolActivityOutcome::Requested {
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("action".to_owned()),
                    CborValue::Text("load".to_owned()),
                ),
                (
                    CborValue::Text("name".to_owned()),
                    CborValue::Text("jujutsu".to_owned()),
                ),
            ]),
        },
    });
    assert_eq!(
        format_session_entry(&skill_request),
        "tool.request skill jujutsu"
    );

    let skill_search = SessionEntry::ToolActivity(ToolActivityRecord {
        call_id: "call_search".into(),
        tool_name: "skill".into(),
        outcome: ToolActivityOutcome::Requested {
            arguments: CborValue::Map(vec![
                (
                    CborValue::Text("action".to_owned()),
                    CborValue::Text("search".to_owned()),
                ),
                (
                    CborValue::Text("query".to_owned()),
                    CborValue::Text("commit".to_owned()),
                ),
            ]),
        },
    });
    assert_eq!(
        format_session_entry(&skill_search),
        "tool.request skill search commit"
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
fn session_and_policy_lines_are_printable() {
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

    let _ = send_daemon_message_with_trace(&sock, "s1", "hello").expect("ok");
    server.join().expect("join").expect("clean exit");

    let sl = session_lines(&sp, "s1").expect("lines");
    assert!(sl.iter().any(|l| l.contains("user: hello")));
    assert!(sl.iter().any(|l| l.contains("tool.request echo")));
    let sll = session_list_lines(&sp).expect("list");
    assert!(sll.iter().any(|l| l.contains("s1 (5 entries)")));
    let pl = policy_lines(sp.join("policy.cbor")).expect("policy");
    assert!(pl.iter().any(|l| l.contains("socket-ui")));
}

#[test]
fn empty_session_and_policy_views() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    std::fs::create_dir_all(&sp).expect("mkdir");
    assert_eq!(session_list_lines(&sp).expect("ok"), vec!["no sessions"]);
    assert_eq!(
        policy_lines(sp.join("policy.cbor")).expect("ok"),
        vec!["no policy approvals"]
    );
    assert_eq!(
        session_lines(&sp, "x").expect("ok"),
        vec!["session x not found"]
    );
}
