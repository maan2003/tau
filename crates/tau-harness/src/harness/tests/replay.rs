use super::*;

fn assistant_output(text: &str) -> Vec<tau_proto::ContextItem> {
    vec![tau_proto::ContextItem::Message(tau_proto::MessageItem {
        role: tau_proto::ContextRole::Assistant,
        content: vec![tau_proto::ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
    })]
}

fn provider_response_contains_text(finished: &ProviderResponseFinished, needle: &str) -> bool {
    finished.output_items.iter().any(|item| {
        matches!(
            item,
            tau_proto::ContextItem::Message(tau_proto::MessageItem { content, .. })
                if content.iter().any(|part| {
                    matches!(part, tau_proto::ContentPart::Text { text } if text.contains(needle))
                })
        )
    })
}

#[test]
fn late_joining_ui_client_receives_replayed_agent_message_exact_selector() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.store
        .append_session_event(
            "s1",
            Some(HARNESS_CONNECTION_ID.into()),
            Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                session_id: "s1".into(),
                agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
            }),
        )
        .expect("seed session membership");
    h.agent_store
        .append_agent_event(
            "agent-1",
            Some(HARNESS_CONNECTION_ID.into()),
            Event::AgentMessageSent(tau_proto::AgentMessageSent {
                message_id: "test-message".into(),
                sender_id: crate::parse_agent_id("agent-1"),
                recipient: tau_proto::AgentMessageRecipient::User,
                kind: tau_proto::AgentMessageKind::Message,
                message: "persisted hello".to_owned(),
            }),
        )
        .expect("seed agent message");

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
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::AGENT_MESSAGE_SENT,
            )],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut got_message = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !got_message {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        got_message = matches!(
            inner,
            TestProtocolItem::Event(Event::AgentMessageSent(message))
                if message.sender_id.as_str() == "agent-1"
                    && message.recipient == tau_proto::AgentMessageRecipient::User
                    && message.message == "persisted hello"
        );
    }

    assert!(got_message, "late UI should replay durable agent messages");

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_receives_replayed_session_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.send_user_message("s1", "hello replay", None)
        .expect("send message");

    let events = loaded_agent_events(&h, "s1");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::AgentPromptSubmitted(_))),
        "user prompt should be in a durable loaded-agent event log"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ProviderResponseFinished(_))),
        "final agent response should be in a durable loaded-agent event log"
    );
    assert!(
        events.iter().all(|event| !event.defaults_to_transient()),
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
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Prefix("session.".to_owned()),
                EventSelector::Prefix("agent.".to_owned()),
                EventSelector::Prefix("provider.".to_owned()),
            ],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut got_session_started = false;
    let mut got_agent_started = false;
    let mut got_prompt = false;
    let mut got_response = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !(got_session_started && got_agent_started && got_prompt && got_response)
    {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        match inner {
            TestProtocolItem::Event(Event::SessionStarted(started))
                if started.session_id.as_str() == "s1" =>
            {
                got_session_started = true;
            }
            TestProtocolItem::Event(Event::AgentStarted(_)) => {
                got_agent_started = true;
            }
            TestProtocolItem::Event(Event::AgentPromptSubmitted(prompt))
                if prompt.text == "hello replay" =>
            {
                got_prompt = true;
            }
            TestProtocolItem::Event(Event::ProviderResponseFinished(finished))
                if finished.output_items.iter().any(|item| {
                    matches!(
                        item,
                        tau_proto::ContextItem::Message(tau_proto::MessageItem { content, .. })
                            if matches!(&content[0], tau_proto::ContentPart::Text { text }
                                if text.contains("hello replay"))
                    )
                }) =>
            {
                got_response = true;
            }
            _ => {}
        }
    }

    assert!(
        got_session_started,
        "late UI should replay current session start"
    );
    assert!(
        got_agent_started,
        "late UI should replay agent display metadata"
    );
    assert!(got_prompt, "late UI should replay prior user prompt");
    assert!(got_response, "late UI should replay prior agent response");

    h.shutdown().expect("shutdown");
}

/// Returns the delivery wrapper for frames carrying an event payload.
fn peel_delivery(message: &HarnessOutputMessage) -> Option<&tau_proto::EventDelivery> {
    match message {
        HarnessOutputMessage::Deliver(delivery) => Some(delivery),
        _ => None,
    }
}

/// Extension subscriptions share the UI late-join path: a late extension is
/// caught up with selector-matched durable facts, delivered as replay-marked
/// frames so side-effecting consumers can distinguish history from live
/// occurrences. This replaced the older live-only rule, whose protection
/// (e.g. std-notifications not replaying sounds) now lives in the replay
/// marker instead of withheld delivery.
#[test]
fn extension_subscribe_replays_durable_facts_as_replay_frames() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let past_text = "past extension replay guard";
    h.send_user_message("s1", past_text, None)
        .expect("send past message");

    let durable_events = loaded_agent_events(&h, "s1");
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ProviderResponseFinished(finished)
                if provider_response_contains_text(finished, past_text))
        }),
        "test setup: past provider response should be durable and eligible for replay",
    );

    let extension_events = connect_test_tool(&mut h, "late-extension");
    h.handle_extension_message(
        "late-extension",
        TestMessage::Subscribe(Subscribe {
            selectors: vec![EventSelector::Exact(
                tau_proto::EventName::PROVIDER_RESPONSE_FINISHED,
            )],
        }),
    )
    .expect("extension subscribe");

    {
        let events = extension_events.lock().expect("sink");
        assert!(
            events.iter().any(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    delivery.is_replay()
                        && matches!(
                            delivery.event(),
                            Event::ProviderResponseFinished(finished)
                                if provider_response_contains_text(finished, past_text)
                        )
                })
            }),
            "late extension should receive the past provider response as a replay frame",
        );
    }

    let live_text = "future live extension event";
    h.send_user_message("s1", live_text, None)
        .expect("send live message");

    {
        let events = extension_events.lock().expect("sink");
        assert!(
            events.iter().any(|routed| {
                peel_delivery(&routed.frame).is_some_and(|delivery| {
                    !delivery.is_replay()
                        && matches!(
                            delivery.event(),
                            Event::ProviderResponseFinished(finished)
                                if provider_response_contains_text(finished, live_text)
                        )
                })
            }),
            "live provider responses must not be marked as replay",
        );
    }

    h.shutdown().expect("shutdown");
}

/// A late extension subscribe announces the current session state — the same
/// `SessionStarted`/`SessionAgentLoaded` snapshot a late UI gets. This is what
/// lets a respawned extension rebuild per-agent state (e.g. agent context)
/// after a mid-session crash instead of rejoining with amnesia.
#[test]
fn extension_subscribe_announces_current_session_snapshot() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Load an agent into the session before the extension subscribes.
    h.send_user_message("s1", "hello snapshot", None)
        .expect("send message");

    let extension_events = connect_test_tool(&mut h, "respawned-extension");
    h.handle_extension_message(
        "respawned-extension",
        TestMessage::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::SESSION_STARTED),
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
            ],
        }),
    )
    .expect("extension subscribe");

    let events = extension_events.lock().expect("sink");
    assert!(
        events.iter().any(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::SessionStarted(started)) if started.session_id.as_str() == "s1"
            )
        }),
        "late extension should be told the current session",
    );
    assert!(
        events.iter().any(|routed| {
            matches!(
                peel_inner_event(&routed.frame),
                Some(Event::SessionAgentLoaded(loaded)) if loaded.session_id.as_str() == "s1"
            )
        }),
        "late extension should be told about already-loaded agents",
    );
    drop(events);

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_and_recalled_prompt_lifecycle_is_not_durable() {
    // Queue/recall state is process-local scheduler/UI state. Persisting only
    // part of that lifecycle makes process restart resurrect prompts that already
    // dispatched or were recalled, so both events must remain transient.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");

    h.publish_event(
        None,
        Event::AgentPromptQueued(AgentPromptQueued {
            agent_id: crate::parse_agent_id(&agent_id),
            text: "edit me".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    h.publish_event(
        None,
        Event::AgentPromptRecalled(AgentPromptRecalled {
            agent_id: crate::parse_agent_id(&agent_id),
            text: "edit me".to_owned(),
        }),
    );

    let events = h.store.session_events("s1").expect("session events");
    assert!(
        events.iter().all(|entry| !matches!(
            entry.event,
            Event::AgentPromptQueued(_) | Event::AgentPromptRecalled(_)
        )),
        "cold replay must not resurrect transient queue lifecycle events: {events:?}"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_replays_final_but_not_stale_queued_session_events() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    let spid: AgentPromptId = "sp-replay".into();
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    let session_id = h.agents[&cid].session_id.clone();
    h.prompt_agents.insert(spid.clone(), cid.clone());
    h.publish_event(
        None,
        Event::AgentPromptQueued(AgentPromptQueued {
            agent_id: crate::parse_agent_id(&agent_id),
            text: "queued for reconnect".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    h.publish_event(
        None,
        Event::AgentPromptCreated(AgentPromptCreated {
            agent_id: crate::parse_agent_id(&agent_id),
            agent_prompt_id: spid.clone(),
            session_id: session_id.clone(),
            system_prompt: String::new(),
            context: tau_proto::PromptContext { blocks: Vec::new() }, // Vec::new(),
            tools: Vec::new(),
            tools_ref: None,
            model: "test/model".parse().expect("model id"),
            model_params: Default::default(),
            tool_choice: Default::default(),
            originator: Default::default(),
            compaction: None,

            share_user_cache_key: false,
            ctx_id: None,
        }),
    );
    h.publish_event(
        None,
        Event::ProviderResponseUpdated(ProviderResponseUpdated {
            agent_prompt_id: spid.clone(),
            items: Vec::new(),
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            originator: Default::default(),
        }),
    );
    h.publish_event(
        None,
        Event::AgentCompactionTriggered(tau_proto::AgentCompactionTriggered {
            agent_id: crate::parse_agent_id(&agent_id),
            originator: tau_proto::PromptOriginator::User,
        }),
    );
    h.publish_event(
        None,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            agent_prompt_id: spid,
            agent_id: crate::parse_agent_id(&agent_id),
            output_items: assistant_output("final"),
            stop_reason: tau_proto::ProviderStopReason::EndTurn,
            error: None,
            originator: Default::default(),
            usage: None,
            compaction_original_input_tokens: None,
            compaction_compacted_input_tokens: None,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
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
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Prefix("agent.".to_owned()),
                EventSelector::Prefix("provider.".to_owned()),
            ],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut replayed = Vec::new();
    while let Ok(Some(frame)) = reader.read_frame() {
        let inner = frame.into_event_frame();
        if let TestProtocolItem::Event(event) = inner {
            replayed.push(event.name());
        }
    }

    assert!(replayed.contains(&tau_proto::EventName::PROVIDER_RESPONSE_FINISHED));
    assert!(replayed.contains(&tau_proto::EventName::AGENT_COMPACTION_TRIGGERED));
    assert!(!replayed.contains(&tau_proto::EventName::AGENT_PROMPT_QUEUED));
    assert!(!replayed.contains(&tau_proto::EventName::AGENT_PROMPT_CREATED));
    assert!(!replayed.contains(&tau_proto::EventName::PROVIDER_RESPONSE_UPDATED));

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_replays_only_current_active_queue() {
    // Queue lifecycle events are transient; durable replay must not resurrect
    // prompts that already dispatched. A late UI still needs the harness's
    // current in-memory queue so it can show prompts that are actually pending.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");
    h.agents
        .get_mut(&cid)
        .expect("default conversation")
        .pending_prompts
        .push_back(crate::agent::PendingPrompt::user("still queued".to_owned()));

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
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
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            selectors: vec![EventSelector::Prefix("agent.".to_owned())],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut queued = Vec::new();
    while let Ok(Some(frame)) = reader.read_frame() {
        let inner = frame.into_event_frame();
        if let TestProtocolItem::Event(Event::AgentPromptQueued(event)) = inner {
            queued.push(event);
        }
    }

    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "still queued");
    assert_eq!(queued[0].agent_id.as_str(), agent_id);

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_replays_terminal_tool_events() {
    // Background completions and cancellation are terminal UI facts. A
    // late UI needs them to clear running tool blocks that were created
    // from earlier live progress before the UI joined.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let cid = ensure_test_user_agent(&mut h);
    let agent_id = h
        .ensure_agent_id_for_agent(&cid)
        .expect("default conversation has an agent id");

    // Seed known tool calls in the agent transcript before recording terminal
    // UI facts for them. Background completions are separate facts that arrive
    // after the foreground round has already been closed with a placeholder.
    let seed_tool_call = |h: &mut Harness, spid: &str, call_id: &str, tool_name: &str| {
        h.publish_for_agent(
            &cid,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                agent_prompt_id: spid.into(),
                agent_id: crate::parse_agent_id(&agent_id),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: call_id.into(),
                    name: ToolName::new(tool_name),
                    tool_type: tau_proto::ToolType::Function,
                    arguments: CborValue::Map(Vec::new()),
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                error: None,
                originator: Default::default(),
                usage: None,
                compaction_original_input_tokens: None,
                compaction_compacted_input_tokens: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        );
    };

    seed_tool_call(
        &mut h,
        "sp-background-result",
        "background-result-call",
        "background_ok",
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            call_id: "background-result-call".into(),
            tool_name: ToolName::new("background_ok"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("running".to_owned()),
            kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
            originator: Default::default(),

            display: None,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
            call_id: "background-result-call".into(),
            tool_name: ToolName::new("background_ok"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("done".to_owned()),
            originator: Default::default(),

            display: None,
        }),
    );

    seed_tool_call(
        &mut h,
        "sp-background-error",
        "background-error-call",
        "background_err",
    );
    h.publish_for_agent(
        &cid,
        Event::ProviderToolResult(ToolResult {
            call_id: "background-error-call".into(),
            tool_name: ToolName::new("background_err"),
            tool_type: tau_proto::ToolType::Function,
            result: CborValue::Text("running".to_owned()),
            kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
            originator: Default::default(),

            display: None,
        }),
    );
    h.publish_for_agent(
        &cid,
        Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
            call_id: "background-error-call".into(),
            tool_name: ToolName::new("background_err"),
            tool_type: tau_proto::ToolType::Function,
            message: "failed after backgrounding".to_owned(),
            details: None,
            originator: Default::default(),

            display: None,
        }),
    );

    seed_tool_call(&mut h, "sp-cancelled", "cancelled-call", "cancel_me");
    h.publish_for_agent(
        &cid,
        Event::ToolCancelled(tau_proto::ToolCancelled {
            call_id: "cancelled-call".into(),
            tool_name: ToolName::new("cancel_me"),
            tool_type: tau_proto::ToolType::Function,
        }),
    );

    let durable_events = loaded_agent_events(&h, "s1");
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ToolBackgroundResult(result)
                if result.call_id.as_str() == "background-result-call")
        }),
        "background result should be in a durable loaded-agent event log"
    );
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "background-error-call")
        }),
        "background error should be in a durable loaded-agent event log"
    );
    assert!(
        durable_events.iter().any(|event| {
            matches!(event, Event::ToolCancelled(cancelled)
                if cancelled.call_id.as_str() == "cancelled-call")
        }),
        "cancellation should be in a durable loaded-agent event log"
    );

    let (server_end, client_end) = UnixStream::pair().expect("pair");
    client_end
        .set_read_timeout(Some(Duration::from_millis(200)))
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
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
        })),
    )
    .expect("subscribe");

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut got_background_result = false;
    let mut got_background_error = false;
    let mut got_cancelled = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && !(got_background_result && got_background_error && got_cancelled)
    {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        let TestProtocolItem::Event(event) = inner else {
            continue;
        };
        match event {
            Event::ToolBackgroundResult(result)
                if result.call_id.as_str() == "background-result-call" =>
            {
                got_background_result = true;
            }
            Event::ToolBackgroundError(error)
                if error.call_id.as_str() == "background-error-call" =>
            {
                got_background_error = true;
            }
            Event::ToolCancelled(cancelled) if cancelled.call_id.as_str() == "cancelled-call" => {
                got_cancelled = true;
            }
            _ => {}
        }
    }

    assert!(
        got_background_result,
        "late UI should replay background tool result"
    );
    assert!(
        got_background_error,
        "late UI should replay background tool error"
    );
    assert!(got_cancelled, "late UI should replay tool cancellation");

    h.shutdown().expect("shutdown");
}

#[test]
fn late_joining_ui_client_does_not_replay_runtime_extension_setup() {
    // Extension discovery/context-ready events are runtime setup facts. The
    // durable replay path now comes from session membership plus loaded-agent
    // transcripts, so these extension events should neither land in the
    // membership log nor be replayed from a transcript to late UI clients.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_conn = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Inject synthetic discovery events as if ext-shell had reported
    // them during eager init. They should remain runtime-only events.
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
            agent_id: tau_proto::AgentId::parse("agent-1").expect("agent id"),
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
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            selectors: vec![EventSelector::Prefix("extension.".to_owned())],
        })),
    )
    .expect("subscribe");

    let session_events = h
        .store
        .session_events(h.current_session_id.as_str())
        .expect("events");
    assert!(
        session_events.iter().all(|e| !matches!(
            &e.event,
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_)
        )),
        "runtime extension setup must not be persisted in the session membership log"
    );

    let mut reader = TestOutputReader::new(BufReader::new(client_end));
    let mut agents_md_count = 0;
    let mut context_ready_count = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let Ok(Some(frame)) = reader.read_frame() else {
            break;
        };
        let inner = frame.into_event_frame();
        let TestProtocolItem::Event(inner) = inner else {
            continue;
        };
        match inner {
            Event::ExtAgentsMdAvailable(a)
                if a.file_path == std::path::Path::new("/test/AGENTS.md") =>
            {
                agents_md_count += 1;
            }
            Event::ExtensionContextReady(_) => {
                context_ready_count += 1;
            }
            _ => {}
        }
    }
    assert_eq!(
        agents_md_count, 0,
        "runtime agents_md setup should not replay to late UI clients"
    );
    assert_eq!(
        context_ready_count, 0,
        "runtime context-ready setup should not replay to late UI clients"
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
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "first");

    let spid1 = h.send_prompt_to_agent("s1");
    h.handle_provider_response_finished(ProviderResponseFinished {
        agent_prompt_id: spid1,
        agent_id: tau_proto::AgentId::parse("main").expect("agent id"),
        output_items: assistant_output("answer"),
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        error: None,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        compaction_original_input_tokens: None,
        compaction_compacted_input_tokens: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("persist agent response");

    append_user_message_via_event(&mut h, "s1", "second");
    let spid2 = h.send_prompt_to_agent("s1");
    let prompt2 = read_prompt_created(&h, &spid2);
    let serialized = serde_json::to_string(&prompt2.context.flatten()).expect("json");
    assert!(
        !serialized.contains("The user is asking"),
        "prompt replay must not echo reasoning summary back to the model",
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn replay_emits_latest_agent_metadata_before_session_agent_loaded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    {
        let sessions_dir = tau_config::settings::sessions_dir_of(&sp);
        let mut sessions = tau_core::SessionStore::open(&sessions_dir).expect("session store");
        sessions
            .append_session_event(
                "s1",
                None,
                Event::SessionAgentLoaded(tau_proto::SessionAgentLoaded {
                    session_id: "s1".into(),
                    agent_id: crate::parse_agent_id("agent-replay-meta"),
                }),
            )
            .expect("seed session membership");
        let mut agents = tau_core::AgentStore::open(sp.join("agents")).expect("agent store");
        agents
            .append_agent_event(
                "agent-replay-meta",
                None,
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: crate::parse_agent_id("agent-replay-meta"),
                    key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                    value: CborValue::Text("/first".to_owned()),
                    inheritable: true,
                }),
            )
            .expect("seed first metadata");
        agents
            .append_agent_event(
                "agent-replay-meta",
                None,
                Event::AgentMetadataSet(tau_proto::AgentMetadataSet {
                    agent_id: crate::parse_agent_id("agent-replay-meta"),
                    key: tau_proto::AgentMetadataKey::new("ext_core-shell_cwd"),
                    value: CborValue::Text("/latest".to_owned()),
                    inheritable: true,
                }),
            )
            .expect("seed latest metadata");
    }

    let mut h = echo_harness_for("s1", &sp).expect("resume");
    let sink = connect_test_client(&mut h, "metadata-ui", tau_proto::ClientKind::Ui);
    h.handle_client_event(
        "metadata-ui",
        TestProtocolItem::Message(TestMessage::Subscribe(Subscribe {
            selectors: vec![
                EventSelector::Exact(tau_proto::EventName::AGENT_METADATA_SET),
                EventSelector::Exact(tau_proto::EventName::SESSION_AGENT_LOADED),
            ],
        })),
    )
    .expect("subscribe");

    let replayed: Vec<Event> = sink
        .lock()
        .expect("sink")
        .iter()
        .filter_map(|routed| peel_inner_event(&routed.frame).cloned())
        .collect();
    let metadata_index = replayed
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::AgentMetadataSet(set)
                    if set.agent_id.as_str() == "agent-replay-meta"
                        && set.key.as_str() == "ext_core-shell_cwd"
                        && set.value == CborValue::Text("/latest".to_owned())
            )
        })
        .expect("latest metadata replayed");
    let loaded_index = replayed
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::SessionAgentLoaded(loaded)
                    if loaded.agent_id.as_str() == "agent-replay-meta"
            )
        })
        .expect("session loaded replayed");
    assert!(metadata_index < loaded_index);
    assert!(replayed.iter().all(|event| !matches!(
        event,
        Event::AgentMetadataSet(set) if set.value == CborValue::Text("/first".to_owned())
    )));

    h.shutdown().expect("shutdown");
}
