use super::*;

fn user_text_item(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
    })
}

fn representative_events() -> Vec<Event> {
    vec![
        Event::ToolRegister(ToolRegister {
            tool: ToolSpec {
                name: ToolName::new("echo"),
                model_visible_name: None,
                description: Some("Echo a payload".to_owned()),
                tool_type: ToolType::Function,
                parameters: None,
                format: None,
                enabled_by_default: true,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: None,
        }),
        Event::ToolRequest(ToolRequest {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            arguments: CborValue::Text("hello".to_owned()),
            originator: PromptOriginator::User,
        }),
        Event::ToolInvoke(ToolInvoke {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            arguments: CborValue::Text("hello".to_owned()),
            originator: PromptOriginator::User,
        }),
        Event::ToolResult(ToolResult {
            call_id: "call-1".into(),
            tool_name: ToolName::new("echo"),
            tool_type: ToolType::Function,
            result: CborValue::Text("hello".to_owned()),
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolError(ToolError {
            call_id: "call-1".into(),
            tool_name: ToolName::new("missing_tool"),
            tool_type: ToolType::Function,
            message: "no live provider".to_owned(),
            details: None,
            display: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
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
            ctx_id: None,
        }),
        Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: SessionStartReason::Initial,
        }),
        Event::SessionCompactionStarted(SessionCompactionStarted {
            session_id: "s1".into(),
            originator: PromptOriginator::User,
            original_input_tokens: None,
        }),
        Event::SessionCompactionFinished(SessionCompactionFinished {
            session_id: "s1".into(),
            originator: PromptOriginator::User,
            original_input_tokens: None,
            compacted_input_tokens: None,
            outcome: SessionCompactionOutcome::Succeeded,
            message: None,
        }),
        Event::SessionCompactionRequested(SessionCompactionRequested {
            prompt: SessionPromptCreated {
                session_prompt_id: "sp-compact-1".into(),
                session_id: "s1".into(),
                system_prompt: "You are helpful.".to_owned(),
                context_items: vec![user_text_item("compact this")],
                tools: Vec::new(),
                tools_ref: None,
                model: None,
                model_params: ModelParams::default(),
                tool_choice: ToolChoice::default(),
                originator: PromptOriginator::Extension {
                    name: ExtensionName::new("harness"),
                    query_id: "auto-compact-default".to_owned(),
                },
                share_user_cache_key: false,
                ctx_id: None,
                previous_response_candidate: None,
            },
        }),
        Event::SessionPromptCreated(SessionPromptCreated {
            session_prompt_id: "sp-1".into(),
            session_id: "s1".into(),
            system_prompt: "You are helpful.".to_owned(),
            context_items: vec![user_text_item("hello")],
            tools: vec![ToolDefinition {
                name: ToolName::new("read"),
                model_visible_name: None,
                description: Some("Read a file".to_owned()),
                tool_type: ToolType::Function,
                parameters: None,
                format: None,
            }],
            tools_ref: None,
            model: None,
            model_params: ModelParams::default(),
            tool_choice: ToolChoice::default(),
            originator: PromptOriginator::User,
            ctx_id: None,
            previous_response_candidate: None,
            share_user_cache_key: false,
        }),
        Event::ProviderResponseFinished(ProviderResponseFinished {
            session_prompt_id: "sp-1".into(),
            output_items: vec![ContextItem::Message(MessageItem {
                role: ContextRole::Assistant,
                content: vec![ContentPart::Text {
                    text: "Hi there".to_owned(),
                }],
                phase: None,
            })],
            stop_reason: ProviderStopReason::EndTurn,
            usage: None,
            originator: PromptOriginator::User,

            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
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
        Event::ProviderModelsUpdated(ProviderModelsUpdated {
            models: vec![ProviderModelInfo {
                id: "openai/gpt-4.1".parse().expect("model id"),
                display_name: Some("GPT-4.1".to_owned()),
                default_affinity: 0,
                context_window: 128_000,
                efforts: vec![Effort::Off, Effort::Low, Effort::Medium, Effort::High],
                verbosities: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
                thinking_summaries: vec![ThinkingSummary::Off],
                supports_compaction: false,
            }],
        }),
        Event::HarnessVerbosityChanged(HarnessVerbosityChanged {
            level: Verbosity::Low,
        }),
        Event::HarnessVerbositiesAvailable(HarnessVerbositiesAvailable {
            levels: vec![Verbosity::Low, Verbosity::Medium, Verbosity::High],
        }),
        Event::HarnessThinkingSummaryChanged(HarnessThinkingSummaryChanged {
            level: ThinkingSummary::Concise,
        }),
        Event::HarnessThinkingSummariesAvailable(HarnessThinkingSummariesAvailable {
            levels: vec![
                ThinkingSummary::Off,
                ThinkingSummary::Auto,
                ThinkingSummary::Concise,
                ThinkingSummary::Detailed,
            ],
        }),
        Event::UiRoleUpdate(UiRoleUpdate {
            role: "engineer".to_owned(),
            action: UiRoleUpdateAction::SetVerbosity {
                verbosity: Some(Verbosity::High),
            },
        }),
        Event::UiRoleUpdate(UiRoleUpdate {
            role: "engineer".to_owned(),
            action: UiRoleUpdateAction::SetThinkingSummary {
                thinking_summary: Some(ThinkingSummary::Auto),
            },
        }),
    ]
}

fn representative_messages() -> Vec<Message> {
    vec![
        Message::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "provider".into(),
            client_kind: ClientKind::Provider,
        }),
        Message::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED),
                EventSelector::Prefix("tool.".to_owned()),
            ],
        }),
        Message::Intercept(Intercept {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
            priority: InterceptionPriority::new(0),
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
        }),
        Message::InterceptRequest(InterceptRequest {
            event: Box::new(Event::SessionStarted(SessionStarted {
                session_id: "s1".into(),
                reason: SessionStartReason::Initial,
            })),
            transient: false,
        }),
        Message::InterceptReply(InterceptReply {
            action: InterceptAction::Pass(None),
        }),
        Message::GetRenderedSystemPrompt(GetRenderedSystemPrompt {
            request_id: "render-prompt-1".to_owned(),
            role: "engineer".to_owned(),
        }),
        Message::RenderedSystemPromptResult(Box::new(RenderedSystemPromptResult {
            request_id: "render-prompt-1".to_owned(),
            prompt: Some("You are helpful.".to_owned()),
            error: None,
        })),
        Message::LogEvent(LogEvent {
            id: LogEventId::new(42),
            recorded_at: UnixMicros::new(1_700_000_000_000_000),
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
        client_name: "provider".into(),
        client_kind: ClientKind::Provider,
    });
    let json = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(json["message"], "hello");
    assert!(json.get("payload").is_some());
}

#[test]
fn event_wire_form_uses_dotted_event_tag() {
    let event = Event::ToolInvoke(ToolInvoke {
        call_id: "call-1".into(),
        tool_name: ToolName::new("echo"),
        arguments: CborValue::Text("hi".to_owned()),
        originator: PromptOriginator::User,
    });
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "tool.invoke");
    assert!(json.get("payload").is_some());
}

#[test]
fn provider_models_updated_name_matches_wire_family() {
    // `provider.models_updated` is routed by event name, so `Event::name()` must
    // match the serde tag exactly. A past implementation accidentally reported
    // this as `extension.provider_models_updated`, which made prefix selectors
    // and debug output disagree with the wire protocol.
    let event = Event::ProviderModelsUpdated(ProviderModelsUpdated { models: Vec::new() });

    assert_eq!(event.name().to_string(), "provider.models_updated");
    let json = serde_json::to_value(&event).expect("serialize");
    assert_eq!(json["event"], "provider.models_updated");
}

#[test]
fn execution_events_use_provider_wire_family() {
    // Provider extensions own execution in the new architecture. Keep the old
    // payload structs during the transition, but hard-switch the public event
    // names so subscribers stop depending on the legacy `agent.*` family.
    let cases = [
        (
            Event::ProviderPromptSubmitted(ProviderPromptSubmitted {
                session_prompt_id: "sp-1".into(),
                originator: PromptOriginator::User,
            }),
            "provider.prompt_submitted",
        ),
        (
            Event::ProviderResponseUpdated(ProviderResponseUpdated {
                session_prompt_id: "sp-1".into(),
                text: "hello".to_owned(),
                thinking: None,
                originator: PromptOriginator::User,
            }),
            "provider.response_updated",
        ),
        (
            Event::ProviderResponseFinished(ProviderResponseFinished {
                session_prompt_id: "sp-1".into(),
                stop_reason: ProviderStopReason::EndTurn,
                originator: PromptOriginator::User,
                ..ProviderResponseFinished::default()
            }),
            "provider.response_finished",
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(event.name().to_string(), expected);
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["event"], expected);
    }
}

#[test]
fn harness_role_info_role_description_is_optional_and_round_trips() {
    // Older harnesses only send `description`; the new free-form role metadata
    // must default cleanly while preserving the technical description field.
    let legacy: HarnessRoleInfo = serde_json::from_value(serde_json::json!({
        "name": "engineer",
        "description": "model=openai/gpt-4.1, effort=high"
    }))
    .expect("decode legacy role info");
    assert_eq!(legacy.name, "engineer");
    assert_eq!(legacy.role_description, None);

    let with_description = HarnessRoleInfo {
        name: "deep".to_owned(),
        description: "model=openai/gpt-4.1, effort=xhigh".to_owned(),
        role_description: Some("Deep investigation mode".to_owned()),
    };
    let json = serde_json::to_value(&with_description).expect("serialize role info");
    assert_eq!(json["role_description"], "Deep investigation mode");
    let decoded: HarnessRoleInfo = serde_json::from_value(json).expect("decode role info");
    assert_eq!(decoded, with_description);

    let without_description = serde_json::to_value(HarnessRoleInfo {
        role_description: None,
        ..with_description
    })
    .expect("serialize role info without metadata");
    assert!(without_description.get("role_description").is_none());
}

#[test]
fn provider_model_info_requires_context_window() {
    // The harness uses provider snapshots as the only source of model UI
    // metadata, so context windows must be present instead of defaulted.
    let value = serde_json::json!({
        "id": "openai/gpt-4.1",
        "efforts": ["off"],
        "verbosities": ["medium"],
        "thinking_summaries": ["off"]
    });

    let error = serde_json::from_value::<ProviderModelInfo>(value)
        .expect_err("context_window should be required");
    assert!(
        error.to_string().contains("context_window"),
        "unexpected error: {error}"
    );
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
fn tool_name_rejects_overlong_input() {
    // ASCII alphanumerics that exceed the cap must be rejected even
    // though they pass the character-class check.
    let long = "a".repeat(ToolName::MAX_LEN + 1);
    assert!(ToolName::try_new(long).is_none());
    let at_cap = "a".repeat(ToolName::MAX_LEN);
    assert!(ToolName::try_new(at_cap).is_some());
}

#[test]
fn frame_peel_log_extracts_log_event_id_and_inner_event() {
    let inner = Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    });
    let frame = Frame::Message(Message::LogEvent(LogEvent {
        id: LogEventId::new(7),
        recorded_at: UnixMicros::new(1_700_000_000_000_000),
        event: Box::new(inner.clone()),
    }));

    let (peeled_id, rest) = frame.peel_log();
    assert_eq!(peeled_id, Some(LogEventId::new(7)));
    assert_eq!(rest, Frame::Event(inner));
}

#[test]
fn frame_peel_log_passes_non_log_frames_through_unchanged() {
    // A bare event must not be mistaken for a log envelope, and the
    // returned frame must be byte-for-byte the same value the caller
    // handed in.
    let event = Event::SessionStarted(SessionStarted {
        session_id: "s1".into(),
        reason: SessionStartReason::Initial,
    });
    let original = Frame::Event(event);
    let (peeled_id, rest) = original.clone().peel_log();
    assert_eq!(peeled_id, None);
    assert_eq!(rest, original);

    // Likewise for a non-LogEvent message.
    let msg = Frame::Message(Message::Ready(Ready {
        message: Some("ready".to_owned()),
    }));
    let (peeled_id, rest) = msg.clone().peel_log();
    assert_eq!(peeled_id, None);
    assert_eq!(rest, msg);
}

#[test]
fn event_defaults_to_transient_marks_progress_kinds() {
    // The set named by `defaults_to_transient` is the contract the
    // harness relies on to decide which events skip the durable
    // session event log when an extension publishes them without
    // explicit transient metadata. Lock it down here so any future
    // edit to the matcher is intentional.
    let transient = [
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            session_prompt_id: "sp-1".into(),
            text: "partial".to_owned(),
            thinking: None,
            originator: PromptOriginator::User,
        }),
        Event::ToolProgress(ToolProgress {
            call_id: "call-1".into(),
            tool_name: ToolName::new("shell"),
            message: Some("running".to_owned()),
            progress: None,
        }),
        Event::UiPromptDraft(UiPromptDraft {
            session_id: "s1".into(),
            text: "draft".to_owned(),
        }),
        Event::SessionCompactionStarted(SessionCompactionStarted {
            session_id: "s1".into(),
            originator: PromptOriginator::User,
            original_input_tokens: None,
        }),
        Event::SessionCompactionFinished(SessionCompactionFinished {
            session_id: "s1".into(),
            originator: PromptOriginator::User,
            original_input_tokens: None,
            compacted_input_tokens: None,
            outcome: SessionCompactionOutcome::Succeeded,
            message: None,
        }),
    ];
    for event in &transient {
        assert!(
            event.defaults_to_transient(),
            "{} should default to transient",
            event.name()
        );
    }

    let durable = [
        Event::SessionStarted(SessionStarted {
            session_id: "s1".into(),
            reason: SessionStartReason::Initial,
        }),
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hi".to_owned(),
            originator: PromptOriginator::User,
            ctx_id: None,
        }),
    ];
    for event in &durable {
        assert!(
            !event.defaults_to_transient(),
            "{} should be durable",
            event.name()
        );
    }
}

/// Tool specs serialize the current shared/exclusive execution-mode field while
/// still accepting the former field name and enum values from older extensions.
#[test]
fn tool_spec_defaults_and_execution_mode_compatibility() {
    let parsed: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "echo",
        "description": "Echo a payload",
        "tool_type": "function",
        "execution_mode": "shared"
    }))
    .expect("deserialize tool spec");
    assert!(parsed.enabled_by_default);
    assert_eq!(parsed.execution_mode, ToolExecutionMode::Shared);

    let legacy_shared: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "legacy_shared",
        "tool_type": "function",
        "side_effects": "pure"
    }))
    .expect("deserialize legacy shared tool spec");
    assert_eq!(legacy_shared.execution_mode, ToolExecutionMode::Shared);

    let legacy_exclusive: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "legacy_exclusive",
        "tool_type": "function",
        "side_effects": "mutating"
    }))
    .expect("deserialize legacy exclusive tool spec");
    assert_eq!(
        legacy_exclusive.execution_mode,
        ToolExecutionMode::Exclusive
    );

    let serialized = serde_json::to_value(&parsed).expect("serialize tool spec");
    assert!(serialized.get("enabled_by_default").is_none());
    assert_eq!(
        serialized["execution_mode"],
        serde_json::Value::String("shared".to_owned())
    );
    assert!(serialized.get("side_effects").is_none());
    assert!(serialized.get("background_support").is_none());
    assert_eq!(parsed.background_support, None);

    let backgrounded: ToolSpec = serde_json::from_value(serde_json::json!({
        "name": "delegate",
        "tool_type": "function",
        "execution_mode": "shared",
        "background_support": "instant"
    }))
    .expect("deserialize background support");
    assert_eq!(
        backgrounded.background_support,
        Some(BackgroundSupport::Instant)
    );

    let disabled = ToolSpec {
        name: ToolName::new("echo"),
        model_visible_name: None,
        description: Some("Echo a payload".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        enabled_by_default: false,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    };
    let serialized = serde_json::to_value(&disabled).expect("serialize disabled tool spec");
    assert_eq!(
        serialized["enabled_by_default"],
        serde_json::Value::Bool(false)
    );
}

/// Prompt fragment primitives are transparent on the wire so config and
/// extension JSON can stay simple: priorities are numbers and prompt contents
/// are plain strings.
#[test]
fn prompt_fragment_primitives_serde_as_simple_values() {
    let priority: PromptPriority =
        serde_json::from_value(serde_json::json!(42)).expect("deserialize prompt priority");
    let content: PromptContent =
        serde_json::from_value(serde_json::json!("Use care")).expect("deserialize prompt content");

    assert_eq!(priority.get(), 42);
    assert_eq!(content.as_str(), "Use care");
    assert_eq!(
        serde_json::to_value(priority).expect("serialize prompt priority"),
        serde_json::json!(42)
    );
    assert_eq!(
        serde_json::to_value(content).expect("serialize prompt content"),
        serde_json::json!("Use care")
    );
}

fn echo_tool_spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::new("echo"),
        model_visible_name: None,
        description: Some("Echo a payload".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    }
}

/// `tool.register` remains compatible with extensions that omit prompt
/// fragments, while newer extensions can attach one ordered prompt fragment.
#[test]
fn tool_register_prompt_is_optional_and_round_trips_when_present() {
    let without_prompt: ToolRegister = serde_json::from_value(serde_json::json!({
        "tool": {
            "name": "echo",
            "description": "Echo a payload",
            "tool_type": "function",
            "execution_mode": "shared"
        }
    }))
    .expect("deserialize tool register without prompt");
    assert_eq!(without_prompt.prompt_fragment, None);

    let with_prompt = ToolRegister {
        tool: echo_tool_spec(),
        prompt_fragment: Some(PromptFragment::new(
            "echo.instructions",
            PromptPriority::new(7),
            "Prefer the echo tool for echo requests.",
        )),
    };
    let json = serde_json::to_value(&with_prompt).expect("serialize tool register with prompt");
    assert_eq!(json["prompt_fragment"]["priority"], serde_json::json!(7));
    assert_eq!(
        json["prompt_fragment"]["template"],
        serde_json::json!("Prefer the echo tool for echo requests.")
    );
    let decoded: ToolRegister = serde_json::from_value(json).expect("decode prompt fragment");
    assert_eq!(decoded, with_prompt);
}

/// Older extensions did not send `execution_mode` or `role` on
/// `ExtAgentQuery`. The harness-owned global sub-agent scheduler must treat the
/// mode as Shared and leave role selection to the harness compatibility path.
#[test]
fn ext_agent_query_execution_mode_defaults_to_shared() {
    let parsed: ExtAgentQuery = serde_json::from_value(serde_json::json!({
        "query_id": "q1",
        "instruction": "summarize"
    }))
    .expect("deserialize ext agent query");
    assert_eq!(parsed.execution_mode, ToolExecutionMode::Shared);
    assert_eq!(parsed.role, None);

    let exclusive: ExtAgentQuery = serde_json::from_value(serde_json::json!({
        "query_id": "q2",
        "instruction": "mutate carefully",
        "execution_mode": "exclusive"
    }))
    .expect("deserialize exclusive ext agent query");
    assert_eq!(exclusive.execution_mode, ToolExecutionMode::Exclusive);
}

/// `DelegateProgress.role` is additive UI metadata. Omitting it must keep older
/// serialized progress events readable by newer clients.
#[test]
fn delegate_progress_role_is_backward_compatible() {
    let parsed: DelegateProgress = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "task_name": "audit",
        "tools_in_flight": 0,
        "tools_total": 0
    }))
    .expect("deserialize progress without role");
    assert_eq!(parsed.role, None);

    let with_role: DelegateProgress = serde_json::from_value(serde_json::json!({
        "call_id": "call-1",
        "task_name": "audit",
        "role": "rush",
        "tools_in_flight": 0,
        "tools_total": 0
    }))
    .expect("deserialize progress with role");
    assert_eq!(with_role.role.as_deref(), Some("rush"));
}

/// `Verbosity::next_in` mirrors `Effort::next_in`. Even though the CLI
/// doesn't bind a cycle key for verbosity today, the helper is part of
/// the public API and the protocol tests should pin the same wrap /
/// skip / empty-allowed-set behaviour effort relies on.
#[test]
fn verbosity_next_in_skips_disallowed_levels_and_wraps() {
    use Verbosity::*;
    let canonical = [Low, Medium, High];

    assert_eq!(Low.next_in(&canonical), Medium);
    assert_eq!(High.next_in(&canonical), Low);

    let only_low_high = [Low, High];
    assert_eq!(Low.next_in(&only_low_high), High);
    assert_eq!(High.next_in(&only_low_high), Low);

    let pinned = [Medium];
    assert_eq!(Low.next_in(&pinned), Medium);
    assert_eq!(Medium.next_in(&pinned), Medium);

    assert_eq!(Medium.next_in(&[]), Medium.next());
}

/// `ThinkingSummary` parses from / displays through the canonical
/// wire forms used by slash commands and harness role config.
#[test]
fn thinking_summary_round_trips_through_display_and_from_str() {
    use ThinkingSummary::*;
    for level in [Off, Auto, Concise, Detailed] {
        let s = level.to_string();
        assert_eq!(s.parse::<ThinkingSummary>().ok(), Some(level));
    }
    assert!("bogus".parse::<ThinkingSummary>().is_err());
}

/// `ModelParams` serializes its bundled knobs as a flat object that
/// drops fields at their default value. Lets `harness.yaml`
/// snapshots stay tiny and avoids surprising callers that introspect
/// the wire shape.
#[test]
fn model_params_serializes_skipping_defaults() {
    let json = serde_json::to_value(ModelParams::default()).expect("serialize");
    assert_eq!(json, serde_json::json!({}));

    let json = serde_json::to_value(ModelParams {
        effort: Effort::High,
        verbosity: Verbosity::Low,
        thinking_summary: ThinkingSummary::Concise,
        service_tier: Some(ServiceTier::Fast),
    })
    .expect("serialize");
    assert_eq!(
        json,
        serde_json::json!({
            "effort": "high",
            "thinking_summary": "concise",
            "service_tier": "fast",
        })
    );
}

/// `Effort::next_in` must skip levels that aren't in the harness's
/// allowed set so cycling callers don't trap when (say) `xhigh` is
/// missing for the current model. Locking the behaviour with explicit
/// cases so a future refactor of the cycle helper can't silently
/// regress the UX.
#[test]
fn effort_next_in_skips_disallowed_levels_and_wraps() {
    use Effort::*;
    let canonical = [Off, Minimal, Low, Medium, High];
    let with_xhigh = [Off, Minimal, Low, Medium, High, XHigh];

    // Without xhigh, High wraps back to Off — XHigh is skipped.
    assert_eq!(High.next_in(&canonical), Off);
    // With xhigh, High advances to XHigh and XHigh wraps to Off.
    assert_eq!(High.next_in(&with_xhigh), XHigh);
    assert_eq!(XHigh.next_in(&with_xhigh), Off);

    // Sparse allowed set (provider with no reasoning effort) — Off
    // is the only legal level, so any input lands there.
    let only_off = [Off];
    assert_eq!(High.next_in(&only_off), Off);
    assert_eq!(Off.next_in(&only_off), Off);

    // Empty allowed set falls through to plain `next()` so callers
    // that haven't received `HarnessEffortsAvailable` yet still make
    // progress.
    assert_eq!(Medium.next_in(&[]), Medium.next());
}

/// Provider-facing tool responses must use the uniform header/body shape so
/// individual providers do not each invent their own CBOR rendering.
#[test]
fn tool_response_renders_headers_blank_line_and_body() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("path".to_owned()),
            CborValue::Text("/tmp/file".to_owned()),
        ),
        (
            CborValue::Text("total_lines".to_owned()),
            CborValue::Integer(2.into()),
        ),
        (
            CborValue::Text("line-numbered content".to_owned()),
            CborValue::Text("1 hello\n2 world".to_owned()),
        ),
    ]));

    assert_eq!(
        response.render(),
        "path: /tmp/file\ntotal_lines: 2\n\n1 hello\n2 world"
    );
}

#[test]
fn tool_response_renders_output_field_as_body_without_label() {
    let response = ToolResponse::from_cbor(&CborValue::Map(vec![
        (
            CborValue::Text("status".to_owned()),
            CborValue::Integer(0.into()),
        ),
        (
            CborValue::Text("output".to_owned()),
            CborValue::Text("out stdout\nerr stderr".to_owned()),
        ),
    ]));

    assert_eq!(response.render(), "status: 0\n\nout stdout\nerr stderr");
}

#[test]
fn tool_response_leaves_plain_text_as_body_only() {
    let response = ToolResponse::from_cbor(&CborValue::Text("done".to_owned()));

    assert_eq!(response.render(), "done");
}
