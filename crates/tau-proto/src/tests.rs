use super::*;

fn representative_events() -> Vec<Event> {
    vec![
        Event::ToolRegister(ToolRegister {
            tool: ToolSpec {
                name: "echo".into(),
                description: Some("Echo a payload".to_owned()),
                parameters: None,
                side_effects: ToolSideEffects::Pure,
            },
        }),
        Event::ToolRequest(ToolRequest {
            call_id: "call-1".into(),
            tool_name: "echo".into(),
            arguments: CborValue::Text("hello".to_owned()),
        }),
        Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: "echo".into(),
            arguments: CborValue::Text("hello".to_owned()),
        }),
        Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: "echo".into(),
            result: CborValue::Text("hello".to_owned()),
        }),
        Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: "missing_tool".into(),
            message: "no live provider".to_owned(),
            details: None,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: "shell".into(),
            message: Some("running".to_owned()),
            progress: Some(ProgressUpdate {
                current: Some(1),
                total: Some(10),
            }),
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".to_owned(),
            originator: PromptOriginator::User,
        }),
        Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: SessionStartReason::Initial,
        }),
        Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: "You are helpful.".to_owned(),
            messages: vec![ConversationMessage {
                role: ConversationRole::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_owned(),
                }],
            }],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: Some("Read a file".to_owned()),
                parameters: None,
            }],
            model: None,
            effort: Effort::Off,
            thinking_summary: ThinkingSummary::Off,
            originator: PromptOriginator::User,
        }),
        Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-1".into(),
            text: Some("Hi there".to_owned()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            thinking: None,
            originator: PromptOriginator::User,
        }),
        Event::ExtensionStarting(ExtensionStarting {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
        }),
        Event::ExtensionReady(ExtensionReady {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
        }),
        Event::ExtensionExited(ExtensionExited {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
            exit_code: Some(0),
            signal: None,
        }),
        Event::ExtensionRestarting(ExtensionRestarting {
            instance_id: 1.into(),
            extension_name: "shell".into(),
            pid: Some(1234),
            attempt: 2,
            reason: Some("hot reload".to_owned()),
        }),
        Event::ExtSkillAvailable(ExtSkillAvailable {
            name: "brave-search".into(),
            description: "Web search via Brave API".to_owned(),
            file_path: "/home/user/.agents/skills/brave-search/SKILL.md".into(),
            add_to_prompt: true,
        }),
        Event::ExtAgentsMdAvailable(ExtAgentsMdAvailable {
            file_path: "/home/user/src/project/AGENTS.md".into(),
            content: "# Project instructions\n- Run tests".to_owned(),
        }),
        Event::ExtensionContextReady(ExtensionContextReady {
            session_id: "s1".into(),
        }),
        Event::ExtensionEvent(CustomEvent {
            name: "demo.progress".parse().expect("event name"),
            session_id: Some("s1".into()),
            payload: CborValue::Text("working".to_owned()),
        }),
    ]
}

fn representative_messages() -> Vec<Message> {
    vec![
        Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "agent".into(),
            client_kind: ClientKind::Agent,
        }),
        Message::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED),
                EventSelector::Prefix("tool.".to_owned()),
            ],
        }),
        Message::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            priority: InterceptionPriority(0),
        }),
        Message::Ready(Ready {
            message: Some("ready".to_owned()),
        }),
        Message::Disconnect(Disconnect {
            reason: Some("shutdown".to_owned()),
        }),
        Message::Configure(Configure {
            config: CborValue::Null,
        }),
        Message::ConfigError(ConfigError {
            message: "bad config".to_owned(),
        }),
        Message::Emit(Emit {
            event: Box::new(Event::ExtensionEvent(CustomEvent {
                name: "demo.transient_progress".parse().expect("event name"),
                session_id: Some("s1".into()),
                payload: CborValue::Text("working".to_owned()),
            })),
            transient: true,
            interception: None,
        }),
        Message::Intercepted(Intercepted {
            event: Box::new(Event::SessionStarted(SessionStarted {
                session_id: "s1".into(),
                reason: SessionStartReason::Initial,
            })),
            transient: false,
            interception: None,
        }),
        Message::LogEvent(LogEvent {
            id: LogEventId::new(42),
            event: Box::new(Event::SessionStarted(SessionStarted {
                session_id: "s1".into(),
                reason: SessionStartReason::Initial,
            })),
        }),
        Message::Ack(Ack {
            up_to: LogEventId::new(42),
        }),
    ]
}

fn representative_frames() -> Vec<Frame> {
    let mut out: Vec<Frame> = representative_events()
        .into_iter()
        .map(Frame::Event)
        .collect();
    out.extend(representative_messages().into_iter().map(Frame::Message));
    out
}

#[test]
fn event_name_round_trips_from_string() {
    for event in representative_events() {
        let name = event.name();
        let serialized = name.to_string();
        assert_eq!(serialized.parse::<EventName>(), Ok(name));
    }
}

#[test]
fn representative_frames_round_trip_through_cbor() {
    for frame in representative_frames() {
        let encoded = encode_frame_to_vec(&frame).expect("frame should encode");
        let decoded = decode_frame_from_slice(&encoded).expect("frame should decode");
        assert_eq!(decoded, frame);
    }
}

#[test]
fn multiple_frames_can_share_one_stream() {
    let frames = representative_frames();
    let mut writer = FrameWriter::new(Vec::new());
    for frame in &frames {
        writer.write_frame(frame).expect("frame should encode");
    }
    writer.flush().expect("stream should flush");

    let bytes = writer.into_inner();
    let mut reader = FrameReader::new(std::io::Cursor::new(bytes));
    let mut decoded = Vec::new();
    for _ in 0..frames.len() {
        decoded.push(
            reader
                .read_frame()
                .expect("read should succeed")
                .expect("frame should arrive"),
        );
    }

    assert_eq!(decoded, frames);
}

#[test]
fn message_wire_form_uses_flat_message_tag() {
    let msg = Message::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "agent".into(),
        client_kind: ClientKind::Agent,
    });
    let json = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(json["message"], "hello");
    assert!(json.get("payload").is_some());
}

#[test]
fn event_wire_form_uses_dotted_event_tag() {
    let event = Event::ToolInvoke(ToolInvoke {
        call_id: "call-1".into(),
        tool_name: "echo".into(),
        arguments: CborValue::Text("hi".to_owned()),
    });
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "tool.invoke");
    assert!(json.get("payload").is_some());
}

#[test]
fn tool_name_accepts_valid_names() {
    assert!(ToolName::try_new("read").is_some());
    assert!(ToolName::try_new("shell").is_some());
    assert!(ToolName::try_new("my_tool_2").is_some());
    assert!(ToolName::try_new("Echo").is_some());
}

#[test]
fn tool_name_rejects_invalid_names() {
    assert!(ToolName::try_new("").is_none());
    assert!(ToolName::try_new("fs.read").is_none());
    assert!(ToolName::try_new("my tool").is_none());
    assert!(ToolName::try_new("a-b").is_none());
    assert!(ToolName::try_new("tool/name").is_none());
}

#[test]
#[should_panic(expected = "invalid tool name")]
fn tool_name_new_panics_on_invalid() {
    let _ = ToolName::new("bad.name");
}

#[test]
fn tool_name_maybe_classifies_inputs() {
    assert!(matches!(
        ToolNameMaybe::from("read"),
        ToolNameMaybe::Valid(_)
    ));
    assert!(matches!(
        ToolNameMaybe::from(""),
        ToolNameMaybe::Invalid(ref s) if s.is_empty()
    ));
    assert!(matches!(
        ToolNameMaybe::from("fs.read"),
        ToolNameMaybe::Invalid(ref s) if s == "fs.read"
    ));
}

#[test]
fn tool_name_maybe_serializes_as_transparent_string() {
    // The wire format must be a plain string — same bytes as if
    // the field were declared `String`. That's what lets us
    // introduce `ToolNameMaybe` without a protocol bump.
    let valid = ToolNameMaybe::from("read");
    let invalid = ToolNameMaybe::from("bad.name");
    assert_eq!(
        serde_json::to_string(&valid).expect("serialize valid"),
        "\"read\""
    );
    assert_eq!(
        serde_json::to_string(&invalid).expect("serialize invalid"),
        "\"bad.name\""
    );

    // Round-trip via JSON picks the right variant.
    let reparsed: ToolNameMaybe = serde_json::from_str("\"read\"").expect("deserialize valid");
    assert!(matches!(reparsed, ToolNameMaybe::Valid(_)));
    let reparsed: ToolNameMaybe =
        serde_json::from_str("\"bad.name\"").expect("deserialize invalid");
    assert!(matches!(reparsed, ToolNameMaybe::Invalid(_)));
}
