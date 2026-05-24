use std::collections::BTreeMap;

use super::*;
use crate::conversation::PendingPrompt;
use crate::extension::{ExtensionConnectCommand, ExtensionEntry, ExtensionState, spawn_in_process};
use crate::harness::{
    PendingTool, extension_disconnected_tool_call_error_message,
    tool_available_again_notice_prompt, tool_unavailable_notice_prompt,
    unavailable_tool_error_message,
};

fn context_text(item: &ContextItem) -> Option<&str> {
    match item {
        ContextItem::Message(message) => message.content.first().map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        }),
        ContextItem::ToolResult(result) => match &result.output.raw {
            CborValue::Text(text) => Some(text.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn prompt_has_tool(prompt: &SessionPromptCreated, name: &str) -> bool {
    prompt.tools.iter().any(|tool| tool.name == name)
}

fn context_text_count(prompt: &SessionPromptCreated, text: &str) -> usize {
    prompt
        .context_items
        .iter()
        .filter(|item| context_text(item) == Some(text))
        .count()
}

fn session_prompt_text_count(h: &Harness, text: &str) -> usize {
    h.store
        .session_events("s1")
        .expect("session events")
        .iter()
        .filter(|entry| {
            matches!(
                &entry.event,
                Event::UiPromptSubmitted(prompt)
                    if prompt.message_class.is_internal() && prompt.text == text
            )
        })
        .count()
}

fn event_log_contains_source_event(
    h: &Harness,
    source: &str,
    mut predicate: impl FnMut(&Event) -> bool,
) -> bool {
    let mut seq = 0;
    while let Some(entry) = h.event_log.get_next_from(seq) {
        seq = entry.seq + 1;
        if entry.source.as_deref() == Some(source) && predicate(&entry.event) {
            return true;
        }
    }
    false
}

fn prompt_context_contains(prompt: &SessionPromptCreated, needle: &str) -> bool {
    prompt
        .context_items
        .iter()
        .filter_map(context_text)
        .any(|text| text.contains(needle))
}

fn shell_tool_spec(h: &Harness) -> ToolSpec {
    h.registry
        .providers_for("shell")
        .into_iter()
        .find(|provider| provider.tool.name == "shell")
        .expect("shell provider")
        .tool
}

fn unregister_shell(h: &mut Harness) {
    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    h.handle_extension_event(
        &conn_id,
        Frame::Event(Event::ToolUnregister(tau_proto::ToolUnregister {
            tool_name: ToolName::new("shell"),
        })),
    )
    .expect("unregister shell");
}

fn reregister_shell(h: &mut Harness, spec: ToolSpec) {
    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    h.handle_extension_event(
        &conn_id,
        Frame::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: spec,
            prompt_fragment: None,
        })),
    )
    .expect("reregister shell");
}

fn staged_tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: ToolName::new(name),
        model_visible_name: None,
        description: Some(format!("{name} test tool")),
        parameters: None,
        tool_type: tau_proto::ToolType::Function,
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Exclusive,
        background_support: Some(tau_proto::BackgroundSupport::Never),
    }
}

fn staged_provider_model(id: &str) -> tau_proto::ProviderModelInfo {
    tau_proto::ProviderModelInfo {
        id: id.into(),
        display_name: Some("Staged".to_owned()),
        default_affinity: 100,
        context_window: 4_096,
        efforts: vec![tau_proto::Effort::Medium],
        verbosities: vec![tau_proto::Verbosity::Medium],
        thinking_summaries: vec![tau_proto::ThinkingSummary::Auto],
        supports_compaction: false,
    }
}

fn clear_quiet_provider_models(h: &mut Harness) {
    let provider_id = h
        .extension_connection_id("provider")
        .expect("provider")
        .to_owned();
    h.handle_extension_event(
        &provider_id,
        Frame::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated { models: Vec::new() },
        )),
    )
    .expect("clear provider models");
}

fn connect_handshaking_extension(
    h: &mut Harness,
    conn_id: &str,
    kind: tau_proto::ClientKind,
) -> Arc<Mutex<Vec<RoutedFrame>>> {
    let sink = connect_test_client(h, conn_id, kind.clone());
    let connection_id: tau_proto::ConnectionId = conn_id.into();
    h.extensions.insert(
        connection_id.clone(),
        ExtensionEntry {
            name: conn_id.to_owned(),
            instance_id: 42.into(),
            connection_id: connection_id.clone(),
            kind,
            pid: None,
            in_process_thread: None,
            supervised_config: None,
            secrets: BTreeMap::new(),
            restart_attempt: 0,
            state: ExtensionState::Handshaking,
            last_acked: tau_proto::LogEventId::default(),
        },
    );
    h.extension_order.push(connection_id);
    sink
}

fn connect_handshaking_tool(h: &mut Harness, conn_id: &str) -> Arc<Mutex<Vec<RoutedFrame>>> {
    connect_handshaking_extension(h, conn_id, tau_proto::ClientKind::Tool)
}

fn sink_has_tool_invoke(sink: &Arc<Mutex<Vec<RoutedFrame>>>, call_id: &str) -> bool {
    sink.lock().expect("sink").iter().any(|routed| {
        matches!(
            peel_inner_event(&routed.frame),
            Some(Event::ToolStarted(invoke)) if invoke.call_id.as_str() == call_id
        )
    })
}

fn test_tool_result(call_id: &str, tool_name: &str) -> Event {
    Event::ToolResult(ToolResult {
        call_id: call_id.into(),
        tool_name: ToolName::new(tool_name),
        tool_type: tau_proto::ToolType::Function,
        result: CborValue::Text("ok".to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    })
}

#[test]
fn configure_includes_extension_state_dir_and_creates_it() {
    // The configure handshake is the only place an extension learns its
    // persistent state location. Keep the path stable at state/ext/<name> and
    // ensure it exists by the time the extension receives it.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_handshaking_tool(&mut h, "std-email");

    h.handle_extension_event(
        "std-email",
        Frame::Message(Message::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "tau-ext-email".into(),
            client_kind: tau_proto::ClientKind::Tool,
        })),
    )
    .expect("hello");

    let frames = sink.lock().expect("sink");
    let configure = frames
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Message(Message::Configure(configure)) => Some(configure),
            _ => None,
        })
        .expect("configure sent");
    let expected =
        tau_config::settings::extension_state_dir_of(&sp, "std-email").expect("safe name");
    assert_eq!(configure.state_dir.as_deref(), Some(expected.as_path()));
    assert!(expected.is_dir(), "{} should exist", expected.display());
}

#[test]
fn configure_includes_only_resolved_extension_secrets() {
    // The lifecycle handshake is the authorization boundary for extension
    // secrets: only the resolved map stored on that extension entry is sent.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let sink = connect_handshaking_tool(&mut h, "std-email");
    h.extensions
        .get_mut("std-email")
        .expect("extension entry")
        .secrets
        .insert(
            "mail_password".to_owned(),
            tau_proto::SecretValue::new("secret"),
        );

    h.handle_extension_event(
        "std-email",
        Frame::Message(Message::Hello(tau_proto::Hello {
            protocol_version: tau_proto::PROTOCOL_VERSION,
            client_name: "tau-ext-email".into(),
            client_kind: tau_proto::ClientKind::Tool,
        })),
    )
    .expect("hello");

    let frames = sink.lock().expect("sink");
    let configure = frames
        .iter()
        .find_map(|routed| match &routed.frame {
            Frame::Message(Message::Configure(configure)) => Some(configure),
            _ => None,
        })
        .expect("configure sent");
    assert_eq!(configure.secrets.len(), 1);
    assert_eq!(configure.secrets["mail_password"].expose_secret(), "secret");
}

#[test]
fn handshaking_tool_register_is_not_active_before_ready() {
    // Capability staging: a tool announced during handshake must not enter the
    // live registry, prompt tool list, or prompt fragments until the extension
    // sends Ready. Tests bypass dispatch gating to verify the assembly inputs
    // directly.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-before-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("staged_tool"),
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "staged_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "STAGED TOOL PROMPT",
            )),
        })),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "staged.extension.instructions",
                    tau_proto::PromptPriority::new(20),
                    "STAGED EXTENSION PROMPT",
                ),
            },
        )),
    )
    .expect("stage extension prompt fragment");

    assert!(h.registry.providers_for("staged_tool").is_empty());
    assert!(
        !h.gather_tool_definitions()
            .iter()
            .any(|tool| tool.name.as_str() == "staged_tool")
    );
    let system_prompt = h.build_current_system_prompt();
    assert!(!system_prompt.contains("STAGED TOOL PROMPT"));
    assert!(!system_prompt.contains("STAGED EXTENSION PROMPT"));

    append_user_message_via_event(&mut h, "s1", "before ready");
    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);
    assert!(!prompt_has_tool(&prompt, "staged_tool"));
    assert!(!prompt.system_prompt.contains("STAGED TOOL PROMPT"));
    assert!(!prompt.system_prompt.contains("STAGED EXTENSION PROMPT"));

    h.shutdown().expect("shutdown");
}

#[test]
fn staged_tool_register_activates_on_ready_and_prompts_include_it() {
    // Ready is the activation boundary: the staged tool and its prompt fragment
    // become visible together before any queued prompts are advanced.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("staged_tool"),
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "staged_tool.instructions",
                tau_proto::PromptPriority::new(10),
                "STAGED TOOL PROMPT",
            )),
        })),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "staged.extension.instructions",
                    tau_proto::PromptPriority::new(20),
                    "STAGED EXTENSION PROMPT",
                ),
            },
        )),
    )
    .expect("stage extension prompt fragment");

    h.handle_extension_message(
        conn_id,
        Message::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert_eq!(h.registry.providers_for("staged_tool").len(), 1);
    append_user_message_via_event(&mut h, "s1", "after ready");
    let spid = h.send_prompt_to_agent("s1");
    let prompt = read_prompt_created(&h, &spid);
    assert!(prompt_has_tool(&prompt, "staged_tool"));
    assert!(prompt.system_prompt.contains("STAGED TOOL PROMPT"));
    assert!(prompt.system_prompt.contains("STAGED EXTENSION PROMPT"));

    h.shutdown().expect("shutdown");
}

#[test]
fn queued_tool_call_waits_for_staged_provider_until_ready() {
    // Regression: a tool call can sit behind another in-flight call while a
    // replacement/late extension is still handshaking. The staged provider must
    // not receive the invoke until Ready, but the queued call should run once
    // the staged registration is activated and still matches the request.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let blocking_sink = connect_test_tool(&mut h, "conn-blocking-tool");
    h.registry
        .register("conn-blocking-tool", staged_tool_spec("blocking_tool"));
    let staged_sink = connect_handshaking_tool(&mut h, "conn-staged-tool");
    h.handle_extension_event(
        "conn-staged-tool",
        Frame::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("staged_tool"),
            prompt_fragment: None,
        })),
    )
    .expect("stage tool");

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-staged-tools");
    h.prompt_conversations
        .insert("sp-staged-tools".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "run two tools".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-staged-tools".into(),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-blocking".into(),
                name: ToolName::new("blocking_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "call-staged".into(),
                name: ToolName::new("staged_tool"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("tool response");

    assert!(sink_has_tool_invoke(&blocking_sink, "call-blocking"));
    assert!(!sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(h.tool_turn.pending_len(), 1);

    h.handle_extension_event(
        "conn-blocking-tool",
        Frame::Event(test_tool_result("call-blocking", "blocking_tool")),
    )
    .expect("blocking result");

    assert!(!sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(h.tool_turn.pending_len(), 1);
    assert_eq!(h.tool_turn.in_flight_len(), 0);

    h.handle_extension_message(
        "conn-staged-tool",
        Message::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(sink_has_tool_invoke(&staged_sink, "call-staged"));
    assert_eq!(
        h.pending_tool_providers
            .get("call-staged")
            .map(|provider| provider.as_str()),
        Some("conn-staged-tool")
    );

    h.handle_extension_event(
        "conn-staged-tool",
        Frame::Event(test_tool_result("call-staged", "staged_tool")),
    )
    .expect("staged result");
    assert!(!h.pending_tool_providers.contains_key("call-staged"));

    h.shutdown().expect("shutdown");
}

#[test]
fn extension_that_never_sends_ready_never_exposes_staged_tool() {
    // A handshaking extension may never finish. Its staged tools must remain
    // unavailable and prompt dispatch stays queued behind the existing Ready
    // gate instead of leaking half-initialized capabilities.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-never-ready";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("never_ready_tool"),
            prompt_fragment: None,
        })),
    )
    .expect("stage tool");

    let submission = h
        .submit_user_prompt("s1".into(), "try never ready tool".to_owned())
        .expect("submit");
    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(h.registry.providers_for("never_ready_tool").is_empty());
    assert!(h.prompt_snapshots.is_empty());

    h.shutdown().expect("shutdown");
}

#[test]
fn provider_models_are_staged_until_ready_and_queued_prompt_waits() {
    // Provider model snapshots define both visible model state and prompt
    // routing. A handshaking provider must not make a queued prompt dispatch
    // until its Ready message activates the staged snapshot.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    clear_quiet_provider_models(&mut h);
    assert!(h.selected_model.is_none());

    let conn_id = "conn-staged-provider";
    let _sink = connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    let model_name = "staged/provider-model";
    let model_id: tau_proto::ModelId = model_name.into();
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![staged_provider_model(model_name)],
            },
        )),
    )
    .expect("stage provider models");

    let submission = h
        .submit_user_prompt("s1".into(), "wait for staged model".to_owned())
        .expect("submit");
    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    assert!(h.prompt_snapshots.is_empty());
    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ProviderModelsUpdated(_))
    }));

    h.handle_extension_message(
        conn_id,
        Message::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(h.available_models.contains(&model_id));
    assert_eq!(
        h.provider_model_routes.get(&model_id).map(|id| id.as_str()),
        Some(conn_id)
    );
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(event, Event::ProviderModelsUpdated(update) if update.models.iter().any(|model| model.id == model_id))
    }));
    let prompt = h
        .prompt_snapshots
        .values()
        .find(|prompt| prompt.model.as_ref() == Some(&model_id))
        .expect("queued prompt dispatched with staged model");
    assert!(prompt_context_contains(prompt, "wait for staged model"));

    h.shutdown().expect("shutdown");
}

#[test]
fn skill_session_context_and_fragment_are_staged_until_ready() {
    // Skills, session context, and extension prompt fragments all feed prompt
    // assembly. None of them may affect the system prompt until Ready activates
    // the staged batch.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());
    let conn_id = "conn-staged-context";
    let _sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: "staged-skill".into(),
            description: "STAGED SKILL DESCRIPTION".to_owned(),
            file_path: "/tmp/staged-skill/SKILL.md".into(),
            add_to_prompt: true,
        })),
    )
    .expect("stage skill");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtSessionContextPublish(
            tau_proto::ExtSessionContextPublish {
                session_id: "s1".into(),
                key: "demo".into(),
                value: tau_proto::SessionContextValue(serde_json::json!({
                    "answer": "STAGED CONTEXT VALUE"
                })),
            },
        )),
    )
    .expect("stage session context");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "staged.context.fragment",
                    tau_proto::PromptPriority::new(20),
                    "CTX={{#each session_context.demo}}{{value.answer}}{{/each}}",
                ),
            },
        )),
    )
    .expect("stage prompt fragment");

    assert!(!h.discovered_skills.contains_key("staged-skill"));
    let before_prompt = h.build_current_system_prompt();
    assert!(!before_prompt.contains("STAGED SKILL DESCRIPTION"));
    assert!(!before_prompt.contains("STAGED CONTEXT VALUE"));

    h.handle_extension_message(
        conn_id,
        Message::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(h.discovered_skills.contains_key("staged-skill"));
    let after_prompt = h.build_current_system_prompt();
    assert!(after_prompt.contains("STAGED SKILL DESCRIPTION"));
    assert!(after_prompt.contains("STAGED CONTEXT VALUE"));

    h.shutdown().expect("shutdown");
}

#[test]
fn agents_context_ready_staged_until_ready_and_queue_waits() {
    // AGENTS.md discovery and the matching context-ready acknowledgement are
    // startup context state. A queued user prompt must wait for Ready, then see
    // the injected AGENTS.md context in the dispatched prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "conn-staged-agents";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    h.initialized_sessions.remove("s1");
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: [tau_proto::ConnectionId::from(conn_id)]
            .into_iter()
            .collect(),
    };

    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtAgentsMdAvailable(
            tau_proto::ExtAgentsMdAvailable {
                file_path: "/repo/AGENTS.md".into(),
                content: "# Rules\nSTAGED AGENTS CONTEXT".to_owned(),
            },
        )),
    )
    .expect("stage agents");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s1".into(),
            },
        )),
    )
    .expect("stage context ready");
    let submission = h
        .submit_user_prompt("s1".into(), "queued after staged context".to_owned())
        .expect("submit");

    assert!(matches!(submission, PromptSubmission::Queued));
    assert!(h.discovered_agents_files.is_empty());
    assert!(matches!(
        h.turn_state,
        TurnState::InitializingSession { .. }
    ));
    assert!(h.prompt_snapshots.is_empty());
    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        matches!(
            event,
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_)
        )
    }));

    h.handle_extension_message(
        conn_id,
        Message::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(h.initialized_sessions.contains("s1"));
    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        matches!(
            event,
            Event::ExtAgentsMdAvailable(_) | Event::ExtensionContextReady(_)
        )
    }));
    let prompt = h
        .prompt_snapshots
        .values()
        .find(|prompt| prompt_context_contains(prompt, "queued after staged context"))
        .expect("queued prompt dispatched after Ready");
    assert!(prompt_context_contains(prompt, "STAGED AGENTS CONTEXT"));

    h.shutdown().expect("shutdown");
}

#[test]
fn interceptor_registration_is_staged_until_ready() {
    // Interception is an extension capability: before Ready, matching events
    // must pass through normally; after Ready, the same selector becomes active.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "conn-staged-interceptor";
    let sink = connect_handshaking_tool(&mut h, conn_id);

    h.handle_extension_message(
        conn_id,
        Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        }),
    )
    .expect("stage intercept");
    h.publish_event(None, draft_event("before ready"));
    assert!(
        sink.lock().expect("sink").iter().all(|routed| {
            !matches!(routed.frame, Frame::Message(Message::InterceptRequest(_)))
        })
    );

    h.handle_extension_message(
        conn_id,
        Message::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");
    h.publish_event(None, draft_event("after ready"));

    assert!(sink.lock().expect("sink").iter().any(|routed| {
        matches!(&routed.frame, Frame::Message(Message::InterceptRequest(req))
            if matches!(req.event.as_ref(), Event::UiPromptDraft(draft) if draft.text == "after ready"))
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn extension_emit_and_start_agent_request_are_staged_until_ready() {
    // Generic emits are visible bus state, and StartAgentRequest starts prompt
    // dispatch. Both are held until Ready so a handshaking extension cannot
    // publish or start side-agent work early.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    let conn_id = "conn-staged-emit-query";
    let _sink = connect_handshaking_tool(&mut h, conn_id);
    let custom_name: tau_proto::EventName = "demo.startup_state".parse().expect("event name");

    h.handle_extension_message(
        conn_id,
        Message::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionEvent(tau_proto::CustomEvent {
                name: custom_name.clone(),
                session_id: Some("s1".into()),
                payload: CborValue::Text("STAGED CUSTOM EVENT".to_owned()),
            })),
            transient: false,
        }),
    )
    .expect("stage emit");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::StartAgentRequest(StartAgentRequest {
            query_id: "q-staged".to_owned(),
            agent_id: "test-agent-q-staged".to_owned(),
            instruction: "STAGED START AGENT REQUEST".to_owned(),
            role: None,
            execution_mode: ToolExecutionMode::Shared,
            input_stats: tau_proto::ToolDisplayStats::default(),
            tool_call_id: None,
            task_name: None,
        })),
    )
    .expect("stage query");

    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        event.name() == custom_name
    }));
    assert!(
        !h.conversations
            .keys()
            .any(|cid| cid.as_str().contains("q-staged"))
    );
    assert!(h.prompt_snapshots.is_empty());

    h.handle_extension_message(
        conn_id,
        Message::Ready(tau_proto::Ready {
            message: Some("ready".to_owned()),
        }),
    )
    .expect("ready");

    assert!(event_log_contains_source_event(&h, conn_id, |event| {
        event.name() == custom_name
    }));
    assert!(
        h.conversations
            .keys()
            .any(|cid| cid.as_str().contains("q-staged"))
    );
    assert!(
        h.prompt_snapshots
            .values()
            .any(|prompt| prompt_context_contains(prompt, "STAGED START AGENT REQUEST"))
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnect_before_ready_drops_all_staged_state() {
    // If a handshaking extension goes away, its staged batch is discarded rather
    // than becoming visible through model routes, prompt assembly, interceptors,
    // custom events, or tool routing.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");
    clear_quiet_provider_models(&mut h);
    let conn_id = "conn-drop-staged";
    let sink = connect_handshaking_extension(&mut h, conn_id, tau_proto::ClientKind::Provider);
    let model_name = "staged/drop-model";
    let model_id: tau_proto::ModelId = model_name.into();

    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ToolRegister(tau_proto::ToolRegister {
            tool: staged_tool_spec("dropped_tool"),
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "dropped.tool.fragment",
                tau_proto::PromptPriority::new(10),
                "DROPPED TOOL FRAGMENT",
            )),
        })),
    )
    .expect("stage tool");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ProviderModelsUpdated(
            tau_proto::ProviderModelsUpdated {
                models: vec![staged_provider_model(model_name)],
            },
        )),
    )
    .expect("stage models");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtSkillAvailable(tau_proto::ExtSkillAvailable {
            name: "dropped-skill".into(),
            description: "DROPPED SKILL".to_owned(),
            file_path: "/tmp/dropped/SKILL.md".into(),
            add_to_prompt: true,
        })),
    )
    .expect("stage skill");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtAgentsMdAvailable(
            tau_proto::ExtAgentsMdAvailable {
                file_path: "/repo/DROPPED.md".into(),
                content: "DROPPED AGENTS".to_owned(),
            },
        )),
    )
    .expect("stage agents");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtSessionContextPublish(
            tau_proto::ExtSessionContextPublish {
                session_id: "s1".into(),
                key: "dropped".into(),
                value: tau_proto::SessionContextValue(serde_json::json!("DROPPED CONTEXT")),
            },
        )),
    )
    .expect("stage session context");
    h.handle_extension_event(
        conn_id,
        Frame::Event(Event::ExtPromptFragmentPublish(
            tau_proto::ExtPromptFragmentPublish {
                fragment: tau_proto::PromptFragment::new(
                    "dropped.extension.fragment",
                    tau_proto::PromptPriority::new(20),
                    "DROPPED EXTENSION FRAGMENT",
                ),
            },
        )),
    )
    .expect("stage fragment");
    h.handle_extension_message(
        conn_id,
        Message::Intercept(Intercept {
            selectors: vec![EventSelector::Exact(tau_proto::EventName::UI_PROMPT_DRAFT)],
            priority: InterceptionPriority::new(0),
        }),
    )
    .expect("stage intercept");
    h.handle_extension_message(
        conn_id,
        Message::Emit(tau_proto::Emit {
            event: Box::new(Event::ExtensionEvent(tau_proto::CustomEvent {
                name: "demo.dropped".parse().expect("event name"),
                session_id: Some("s1".into()),
                payload: CborValue::Text("DROPPED EVENT".to_owned()),
            })),
            transient: false,
        }),
    )
    .expect("stage emit");

    h.handle_disconnect(conn_id);
    h.publish_event(None, draft_event("after disconnect"));

    assert!(!h.extension_activation_staging.contains_key(conn_id));
    assert!(h.registry.providers_for("dropped_tool").is_empty());
    assert!(!h.available_models.contains(&model_id));
    assert!(!h.provider_model_routes.contains_key(&model_id));
    assert!(!h.discovered_skills.contains_key("dropped-skill"));
    assert!(h.discovered_agents_files.is_empty());
    assert!(
        !h.session_context
            .template_value(&"s1".into())
            .to_string()
            .contains("DROPPED CONTEXT")
    );
    assert!(!h.build_current_system_prompt().contains("DROPPED"));
    assert!(!event_log_contains_source_event(&h, conn_id, |event| {
        event.name().to_string().contains("dropped")
    }));
    assert!(
        sink.lock().expect("sink").iter().all(|routed| {
            !matches!(routed.frame, Frame::Message(Message::InterceptRequest(_)))
        })
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn tool_unregister_removes_tool_from_future_prompt() {
    // Regression: an explicit ToolUnregister must update the live registry used
    // for future prompt assembly while leaving old prompt snapshots intact.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "before unregister");
    let before_spid = h.send_prompt_to_agent("s1");
    let before_prompt = read_prompt_created(&h, &before_spid);
    assert!(prompt_has_tool(&before_prompt, "shell"));

    unregister_shell(&mut h);

    append_user_message_via_event(&mut h, "s1", "after unregister");
    let after_spid = h.send_prompt_to_agent("s1");
    let after_prompt = read_prompt_created(&h, &after_spid);

    assert!(prompt_has_tool(&before_prompt, "shell"));
    assert!(!prompt_has_tool(&after_prompt, "shell"));

    h.shutdown().expect("shutdown");
}

#[test]
fn old_prompt_call_gets_tau_internal_unavailable_error() {
    // Regression: a prompt that was created before unregister can still contain
    // the old tool definition. If the agent calls it after the provider removed
    // the tool, the harness must close the call with an internal tool error.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    append_user_message_via_event(&mut h, "s1", "use shell");
    let spid = h.send_prompt_to_agent("s1");
    let old_prompt = read_prompt_created(&h, &spid);
    assert!(prompt_has_tool(&old_prompt, "shell"));

    unregister_shell(&mut h);

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: spid,
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("unavailable old tool call should be closed");

    let expected = unavailable_tool_error_message(&ToolName::new("shell"));
    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().any(|node| {
        matches!(
            &node.entry,
            SessionEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message == &expected
                        )
                })
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn unregister_queues_unavailable_notice_for_next_user_prompt_only() {
    // Availability notices are hidden context for the next real user turn, not
    // standalone internal prompts dispatched at unregister time.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));
    unregister_shell(&mut h);

    assert!(h.prompt_snapshots.is_empty());
    assert_eq!(session_prompt_text_count(&h, &notice), 0);

    let cid = h.default_conversation_id.clone();
    h.dispatch_prompt_for_conversation(&cid, PendingPrompt::user("after unregister".to_owned()))
        .expect("dispatch user prompt");

    let prompt = read_prompt_created(&h, &SessionPromptId::from("sp-0"));
    let notice_pos = prompt
        .context_items
        .iter()
        .position(|item| context_text(item) == Some(notice.as_str()))
        .expect("availability notice in prompt");
    let user_pos = prompt
        .context_items
        .iter()
        .position(|item| context_text(item) == Some("after unregister"))
        .expect("user prompt in prompt");
    assert!(notice_pos < user_pos);
    assert_eq!(session_prompt_text_count(&h, &notice), 1);

    h.shutdown().expect("shutdown");
}

#[test]
fn reregister_before_notice_delivery_dequeues_unavailable_notice() {
    // A quick unregister/register pair should be invisible to the model.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));
    unregister_shell(&mut h);
    reregister_shell(&mut h, spec);

    let cid = h.default_conversation_id.clone();
    h.dispatch_prompt_for_conversation(&cid, PendingPrompt::user("after reconnect".to_owned()))
        .expect("dispatch user prompt");

    let prompt = read_prompt_created(&h, &SessionPromptId::from("sp-0"));
    assert_eq!(context_text_count(&prompt, &notice), 0);
    assert_eq!(session_prompt_text_count(&h, &notice), 0);
    assert!(prompt_has_tool(&prompt, "shell"));

    h.shutdown().expect("shutdown");
}

#[test]
fn reregister_after_notice_delivery_queues_available_again_notice() {
    // Once the model has been told a tool disappeared, the matching
    // re-registration needs a hidden available-again notice on the next user
    // turn so the model can trust the refreshed tool list.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    let unavailable = tool_unavailable_notice_prompt(&ToolName::new("shell"));
    let available = tool_available_again_notice_prompt(&ToolName::new("shell"));
    unregister_shell(&mut h);

    let cid = h.default_conversation_id.clone();
    h.dispatch_prompt_for_conversation(&cid, PendingPrompt::user("after unregister".to_owned()))
        .expect("dispatch unavailable prompt");
    let first_prompt = read_prompt_created(&h, &SessionPromptId::from("sp-0"));
    assert_eq!(context_text_count(&first_prompt, &unavailable), 1);

    reregister_shell(&mut h, spec);
    h.dispatch_prompt_for_conversation(&cid, PendingPrompt::user("after reregister".to_owned()))
        .expect("dispatch available prompt");

    let second_prompt = read_prompt_created(&h, &SessionPromptId::from("sp-1"));
    let available_pos = second_prompt
        .context_items
        .iter()
        .position(|item| context_text(item) == Some(available.as_str()))
        .expect("available-again notice in prompt");
    let user_pos = second_prompt
        .context_items
        .iter()
        .position(|item| context_text(item) == Some("after reregister"))
        .expect("user prompt in prompt");
    assert!(available_pos < user_pos);
    assert_eq!(session_prompt_text_count(&h, &available), 1);
    assert!(prompt_has_tool(&second_prompt, "shell"));

    h.shutdown().expect("shutdown");
}

#[test]
fn duplicate_provider_keeps_tool_available_without_notice() {
    // Removing one provider must not hide the tool if another provider for the
    // same tool name remains registered.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let spec = shell_tool_spec(&h);
    h.registry.register("conn-duplicate-shell", spec);
    let notice = tool_unavailable_notice_prompt(&ToolName::new("shell"));

    unregister_shell(&mut h);
    assert_eq!(h.registry.providers_for("shell").len(), 1);

    let cid = h.default_conversation_id.clone();
    h.dispatch_prompt_for_conversation(
        &cid,
        PendingPrompt::user("after partial unregister".to_owned()),
    )
    .expect("dispatch user prompt");

    let prompt = read_prompt_created(&h, &SessionPromptId::from("sp-0"));
    assert_eq!(context_text_count(&prompt, &notice), 0);
    assert_eq!(session_prompt_text_count(&h, &notice), 0);
    assert!(prompt_has_tool(&prompt, "shell"));

    h.shutdown().expect("shutdown");
}

#[test]
fn unavailable_tool_is_reported_without_crashing() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let removed = h.registry.unregister_connection(&conn_id);
    assert!(removed.iter().any(|t| t == "shell"));

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "shell printf hi".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("unavailable tool should be rejected cleanly");

    let expected = unavailable_tool_error_message(&ToolName::new("shell"));
    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().any(|node| {
        matches!(
            &node.entry,
            SessionEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message == &expected
                        )
                })
        )
    }));
    let followup_prompt = read_prompt_created(&h, &SessionPromptId::from("sp-0"));
    assert!(
        followup_prompt
            .context_items
            .iter()
            .any(|item| matches!(item, ContextItem::ToolResult(_))),
        "follow-up prompt should include the persisted tool error as a tool_result item"
    );
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
    let tool_name = ToolName::new("shell");
    let cid = h.default_conversation_id.clone();
    h.publish_for_conversation(
        &cid,
        Event::ProviderResponseFinished(ProviderResponseFinished {
            session_prompt_id: "sp-main".into(),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            })],
            stop_reason: tau_proto::ProviderStopReason::ToolCalls,
            usage: None,
            originator: tau_proto::PromptOriginator::User,
            backend: None,
            provider_response_id: None,
            ws_pool_delta: None,
        }),
    );
    h.tool_conversations.insert(call_id.clone(), cid.clone());
    h.pending_tools.insert(
        call_id.clone(),
        PendingTool {
            name: tool_name.clone(),
            internal_name: tool_name.clone(),
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.pending_tool_providers
        .insert(call_id.clone(), conn_id.clone().into());
    h.tool_turn.record_in_flight_for_test(
        cid.clone(),
        call_id.clone(),
        tau_proto::ToolExecutionMode::Exclusive,
    );
    if let Some(conv) = h.conversations.get_mut(&cid) {
        conv.turn_state = ConversationTurnState::ToolsRunning {
            remaining_calls: vec![call_id.clone()],
        };
    }

    h.handle_disconnect(&conn_id);

    // Disconnect publishes a ToolError, drops the call from the
    // conversation's `ToolsRunning` set, and — since that was the
    // last outstanding call — re-prompts the agent so it can react
    // to the failure. The conversation therefore transitions
    // `ToolsRunning -> AgentThinking`, not back to `Idle`.
    assert!(matches!(h.turn_state, TurnState::Idle));
    assert!(matches!(
        h.conversations
            .get(&h.default_conversation_id)
            .expect("default conversation")
            .turn_state,
        ConversationTurnState::AgentThinking { .. }
    ));
    assert!(!h.tool_conversations.contains_key(&call_id));
    assert!(!h.pending_tool_providers.contains_key(&call_id));

    let expected = extension_disconnected_tool_call_error_message(&call_id);
    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().any(|node| {
        matches!(
            &node.entry,
            SessionEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id == call_id
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message }
                                if message == &expected
                        )
                })
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn disconnected_tool_is_removed_cleanly() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let conn_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();

    // Send disconnect to the extension via the bus (through the
    // writer channel → writer thread → stream).
    let _ = h.bus.send_to(
        &conn_id,
        None,
        Frame::Message(Message::Disconnect(Disconnect {
            reason: Some("test".to_owned()),
        })),
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
                frame,
            } => {
                let _ = h.handle_extension_event(&connection_id, *frame);
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

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "shell printf hi".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("removed tool should be rejected cleanly");

    let expected = unavailable_tool_error_message(&ToolName::new("shell"));
    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().any(|node| {
        matches!(
            &node.entry,
            SessionEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message == &expected
                        )
                })
        )
    }));
    h.shutdown().expect("shutdown");
}

#[test]
fn extension_connect_command_installs_state_before_reader_ack() {
    // Regression: extension spawn helpers used to mutate bus state directly.
    // The reader must stay gated until the harness loop has installed both
    // the bus connection and the lifecycle entry, then emitted the starting
    // barrier.
    fn eager_hello_extension(r: UnixStream, w: UnixStream) -> Result<(), String> {
        let mut writer = FrameWriter::new(BufWriter::new(w));
        writer
            .write_frame(&Frame::Message(Message::Hello(tau_proto::Hello {
                protocol_version: tau_proto::PROTOCOL_VERSION,
                client_name: "late-tool".into(),
                client_kind: tau_proto::ClientKind::Tool,
            })))
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;
        writer
            .write_frame(&Frame::Message(Message::Ready(tau_proto::Ready {
                message: None,
            })))
            .map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())?;

        let mut reader = FrameReader::new(BufReader::new(r));
        while let Some(frame) = reader.read_frame().map_err(|e| e.to_string())? {
            let (_, frame) = frame.peel_log();
            if matches!(frame, Frame::Message(Message::Disconnect(_))) {
                break;
            }
        }
        Ok(())
    }

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = quiet_provider_harness(&sp).expect("start");

    let spawned = spawn_in_process(
        "late-tool",
        tau_proto::ClientKind::Tool,
        eager_hello_extension,
        &h.tx,
    )
    .expect("spawn late tool");
    let conn_id = spawned.connection_id.clone();
    h.queue_extension_connect(ExtensionConnectCommand {
        entry: ExtensionEntry {
            name: "late-tool".to_owned(),
            instance_id: 999.into(),
            connection_id: conn_id.clone(),
            kind: tau_proto::ClientKind::Tool,
            pid: Some(std::process::id()),
            in_process_thread: Some(spawned.thread),
            supervised_config: None,
            secrets: BTreeMap::new(),
            restart_attempt: 0,
            state: ExtensionState::Spawning,
            last_acked: tau_proto::LogEventId::default(),
        },
        origin: ConnectionOrigin::Supervised,
        writer_tx: spawned.writer_tx,
        initialized_ack: spawned.initialized_ack,
        replaces: None,
    })
    .expect("queue connect command");

    assert!(h.bus.connection(&conn_id).is_none());
    assert!(!h.extensions.contains_key(&conn_id));

    let event =
        h.rx.recv_timeout(Duration::from_secs(1))
            .expect("connect command should be first");
    match event {
        HarnessEvent::Command(command) => h.handle_harness_command(command).expect("handle"),
        HarnessEvent::FromConnection { .. }
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::NewClient(_) => panic!("reader forwarded before connect command"),
    }

    assert!(h.bus.connection(&conn_id).is_some());
    assert!(h.extensions.contains_key(&conn_id));
    assert!(
        h.lifecycle_messages
            .iter()
            .any(|m| m == "extension late-tool starting")
    );

    let event =
        h.rx.recv_timeout(Duration::from_secs(1))
            .expect("reader should forward after connect ack");
    match event {
        HarnessEvent::FromConnection {
            connection_id,
            frame,
        } => {
            assert_eq!(connection_id, conn_id);
            assert!(matches!(frame.as_ref(), Frame::Message(Message::Hello(_))));
        }
        HarnessEvent::Command(_)
        | HarnessEvent::Disconnected { .. }
        | HarnessEvent::NewClient(_) => panic!("unexpected harness event after connect ack"),
    }

    h.shutdown().expect("shutdown");
}

#[test]
fn role_disabled_tool_is_reported_without_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let config_dir = td.path().join("config");
    let state_dir = td.path().join("state");
    std::fs::create_dir_all(&config_dir).expect("mkdir config");
    std::fs::create_dir_all(&state_dir).expect("mkdir state");
    std::fs::write(
        config_dir.join("harness.yaml"),
        r#"{
            roleGroups: {
                engineer: {
                    roles: {
                        engineer: { disableTools: ["shell"] },
                    },
                },
            },
        }"#,
    )
    .expect("write harness");
    let dirs = tau_config::settings::TauDirs {
        config_dir: Some(config_dir),
        state_dir: Some(state_dir.clone()),
    };
    let mut h = echo_harness_with_dirs("s1", state_dir, dirs).expect("start");

    h.selected_model = Some("test/model".into());
    h.selected_role = "engineer".to_owned();
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("disabled tool call should be handled");

    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().any(|node| {
        matches!(
            &node.entry,
            SessionEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message }
                                if message == "tool is not enabled for the current role"
                        )
                })
        )
    }));

    h.shutdown().expect("shutdown");
}

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
        reason: tau_proto::SessionStartReason::Initial,
        waiting_on: [tools_connection_id.clone().into()].into_iter().collect(),
    };
    h.handle_extension_event(
        &tools_connection_id,
        Frame::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s1".into(),
            },
        )),
    )
    .expect("ready");

    assert!(matches!(h.turn_state, TurnState::Idle));

    let events = h.store.session_events("s1").expect("session events");
    let injected = events
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            Event::SessionUserMessageInjected(injected)
                if injected.text.contains("# AGENTS.md instructions")
                    && injected.text.contains("/repo/pkg") =>
            {
                Some(injected.text.as_str())
            }
            _ => None,
        })
        .expect("expected injected AGENTS.md user message");
    assert!(injected.contains("# AGENTS.md instructions"));
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/pkg/AGENTS.md\">"));
    assert!(injected.contains("<AGENTS_FILE path=\"/repo/AGENTS.md\">"));
    assert!(injected.contains("</AGENTS_FILE>"));
    let root_pos = injected.find("root rule").expect("root rule");
    let pkg_pos = injected.find("package rule").expect("package rule");
    assert!(
        root_pos < pkg_pos,
        "broader file should appear before nested one"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn resumed_session_init_does_not_reinject_agents_context() {
    // Regression: cold resume must wait for extensions to refresh their
    // context, but the restored conversation already contains the startup
    // AGENTS.md user message. Appending it again makes the model see a
    // duplicate user instruction before the first resumed prompt.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let tools_connection_id = h
        .extension_connection_id("shell")
        .expect("shell")
        .to_owned();
    let marker = "resume AGENTS marker";
    let count_marker_injections = |h: &Harness| -> usize {
        h.store
            .session_events("s1")
            .expect("session events")
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.event,
                    Event::SessionUserMessageInjected(injected)
                        if injected.text.contains(marker)
                )
            })
            .count()
    };

    h.discovered_agents_files.clear();
    let cid = h.default_conversation_id.clone();
    h.publish_event_for_conversation(
        &cid,
        None,
        Event::SessionUserMessageInjected(tau_proto::SessionUserMessageInjected {
            session_id: "s1".into(),
            text: format!("# AGENTS.md instructions\n{marker}"),
            message_class: tau_proto::PromptMessageClass::User,
        }),
    );
    assert_eq!(count_marker_injections(&h), 1);

    h.discovered_agents_files.push(DiscoveredAgentsFile {
        source_id: tools_connection_id.clone().into(),
        file_path: PathBuf::from("/repo/AGENTS.md"),
        content: format!("# Root\n- {marker}\n"),
    });
    h.pending_restore_notice_sessions.insert("s1".into(), None);
    h.turn_state = TurnState::InitializingSession {
        session_id: "s1".into(),
        reason: tau_proto::SessionStartReason::Resume,
        waiting_on: [tools_connection_id.clone().into()].into_iter().collect(),
    };
    h.handle_extension_event(
        &tools_connection_id,
        Frame::Event(Event::ExtensionContextReady(
            tau_proto::ExtensionContextReady {
                session_id: "s1".into(),
            },
        )),
    )
    .expect("ready");

    assert!(matches!(h.turn_state, TurnState::Idle));
    assert_eq!(count_marker_injections(&h), 1);
    assert!(
        h.pending_restore_notice_sessions.contains_key("s1"),
        "restore notice queue should be independent from AGENTS.md injection"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn unavailable_tool_name_does_not_panic_and_surfaces_error() {
    // Valid Tau-visible tool names that cannot be routed are model
    // errors, not malformed transcript structure. Commit the assistant
    // call and add a terminal tool error so the next prompt contains a
    // matched function_call/function_call_output pair.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    // Pre-seed as if the agent had just been prompted and is now
    // responding with tool_calls.
    h.selected_model = Some("test/model".into());
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    h.handle_provider_response_finished(response)
        .expect("invalid tool call must not panic");

    // The call must be gone from both the pending queue and the
    // in-flight set — rejection fully completes it.
    assert!(h.tool_turn.is_empty());

    // The error should have been persisted on s1's history so the
    // agent sees it on the next turn — as a Requested + Error pair
    // under the same call_id, so the Responses-API serializer can
    // emit a matching `function_call` / `function_call_output`
    // without the latter looking unpaired.
    let expected = unavailable_tool_error_message(&ToolName::new("not_a_tool"));
    let session = h.store.session("s1").expect("session");
    let mut saw_call = false;
    let mut saw_error = false;
    for node in session.nodes() {
        match &node.entry {
            SessionEntry::AssistantResponse { output_items, .. } => {
                saw_call |= output_items.iter().any(|item| {
                    matches!(item, ContextItem::ToolCall(call) if call.call_id.as_str() == "c1")
                });
            }
            SessionEntry::ToolResults { items } => {
                saw_error |= items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message }
                                if message == &expected
                        )
                });
            }
            _ => {}
        }
    }
    assert!(
        saw_call && saw_error,
        "rejected call should leave both the assistant tool call and an error result \
         matching tool_use / tool_result pair"
    );

    h.shutdown().expect("shutdown");
}

#[test]
fn empty_tool_call_id_rejects_response_before_commit() {
    // Empty provider call ids cannot be correlated into a durable
    // transcript. Reject them before committing an assistant response
    // or dispatching tools; synthesizing ids would hide malformed
    // provider output and let unsafe tool work proceed.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");

    h.selected_model = Some("test/model".into());
    h.registry.register(
        "conn-delegate",
        ToolSpec {
            name: ToolName::new("delegate"),
            model_visible_name: None,
            description: None,
            parameters: None,
            tool_type: tau_proto::ToolType::Function,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Exclusive,
            background_support: None,
        },
    );
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            target_agent_id: None,
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "".into(),
                name: ToolName::new("delegate"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "".into(),
                name: ToolName::new("delegate"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::ProviderTokenUsage {
                model: None,
                prompt_sent_tokens: input_tokens.unwrap_or(0),
                prompt_cached_tokens: cached_tokens.unwrap_or(0),
                response_received_tokens: output_tokens.unwrap_or(0),
                stats: Default::default(),
            }),
        },
        originator: tau_proto::PromptOriginator::User,

        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    };

    let error = h
        .handle_provider_response_finished(response)
        .expect_err("empty call id must reject the response");
    assert!(
        error.to_string().contains("empty call_id"),
        "unexpected error: {error}"
    );

    // No tool work should be scheduled and the malformed assistant
    // response should not be committed to the session tree.
    assert!(h.tool_turn.is_empty());
    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().all(|node| {
        !matches!(
            node.entry,
            SessionEntry::AssistantResponse { .. } | SessionEntry::ToolResults { .. }
        )
    }));

    h.shutdown().expect("shutdown");
}

#[test]
fn cancel_after_agent_thinking_terminalizes_tool_calls_before_dispatch() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.handle_client_event(
        "ui",
        Frame::Event(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: "s1".into(),
            session_prompt_id: None,
        })),
    )
    .expect("cancel");

    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Null,
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Null,
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");

    assert!(h.tool_turn.is_empty());
    assert!(matches!(
        h.conversations.get(&cid).expect("conversation").turn_state,
        ConversationTurnState::Idle
    ));
    let session = h.store.session("s1").expect("session");
    let cancelled: Vec<_> = session
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            SessionEntry::ToolResults { items } => Some(items.iter()),
            _ => None,
        })
        .flatten()
        .filter(|item| matches!(item.status, ToolResultStatus::Cancelled { .. }))
        .map(|item| item.call_id.as_str().to_owned())
        .collect();
    assert_eq!(cancelled, vec!["c1".to_owned(), "c2".to_owned()]);
}

#[test]
fn cancel_during_tools_terminalizes_inflight_and_queued_calls() {
    use tau_proto::ToolExecutionMode::{Exclusive, Shared};

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.handle_provider_response_finished(ProviderResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c1".into(),
                name: ToolName::new("read"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(vec![(
                    CborValue::Text("path".to_owned()),
                    CborValue::Text("/nonexistent/tau-cancel-test".to_owned()),
                )]),
            }),
            ContextItem::ToolCall(ToolCallItem {
                call_id: "c2".into(),
                name: ToolName::new("write"),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            }),
        ],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");
    assert_eq!(
        h.tool_turn.in_flight_mode(&ToolCallId::from("c1")),
        Some(&Shared)
    );
    let pending = h.tool_turn.pending(0).expect("c2 should be queued");
    assert_eq!(pending.invocation.id, "c2");
    assert_eq!(pending.execution_mode, Exclusive);

    h.handle_client_event(
        "ui",
        Frame::Event(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: "s1".into(),
            session_prompt_id: None,
        })),
    )
    .expect("cancel");

    assert!(h.tool_turn.is_empty());
    assert!(matches!(
        h.conversations.get(&cid).expect("conversation").turn_state,
        ConversationTurnState::Idle
    ));
    let session = h.store.session("s1").expect("session");
    let cancelled: Vec<_> = session
        .nodes()
        .iter()
        .filter_map(|node| match &node.entry {
            SessionEntry::ToolResults { items } => Some(items.iter()),
            _ => None,
        })
        .flatten()
        .filter(|item| matches!(item.status, ToolResultStatus::Cancelled { .. }))
        .map(|item| item.call_id.as_str().to_owned())
        .collect();
    assert_eq!(cancelled, vec!["c1".to_owned(), "c2".to_owned()]);
}

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
        Frame::Message(Message::Ack(tau_proto::Ack {
            up_to: tau_proto::LogEventId::new(7),
        })),
    )
    .expect("ack");

    let tools = h.extensions.get(tools_id.as_str()).expect("entry");
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
        .get(tools_id.as_str())
        .expect("entry")
        .last_acked;

    // Resending an old ack must not move the cursor backward and
    // must not bump it forward either.
    h.handle_extension_event(
        &tools_id,
        Frame::Message(Message::Ack(tau_proto::Ack {
            up_to: tau_proto::LogEventId::new(0),
        })),
    )
    .expect("ack");

    let after = h
        .extensions
        .get(tools_id.as_str())
        .expect("entry")
        .last_acked;
    assert_eq!(before, after, "stale ack should not change cursor");
    h.shutdown().expect("shutdown");
}

#[test]
fn provider_disconnect_terminates_event_loop() {
    // Providers are the only prompt executors now. If the selected provider
    // disconnects, keeping the harness alive would leave any in-flight turn
    // without an execution client and can wedge the UI. Treat provider exit as
    // fatal instead of respawning it like a tool extension.
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    let provider_id = h
        .extension_connection_id("provider")
        .expect("provider")
        .to_owned();

    h.tx.send(HarnessEvent::Disconnected {
        connection_id: provider_id.into(),
    })
    .expect("queue provider disconnect");

    let err = h
        .run_event_loop(None, false)
        .expect_err("provider disconnect should terminate harness");
    assert!(matches!(
        err,
        HarnessError::Participant(message) if message == "provider disconnected"
    ));

    h.shutdown().expect("shutdown");
}

#[test]
fn duplicate_tool_result_is_discarded() {
    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");

    let mut h = echo_harness(&sp).expect("start");

    // Fabricate a tool result for a call_id that is not in pending_tool_sessions.
    let result = h.handle_extension_event(
        "fake-ext",
        Frame::Event(Event::ToolResult(ToolResult {
            call_id: "orphan-call".into(),
            tool_name: ToolName::new("read"),
            tool_type: tau_proto::ToolType::Function,
            result: tau_proto::CborValue::Text("stale data".to_owned()),
            kind: tau_proto::ToolResultKind::Final,
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    );
    // Should not error — just emits a warning and discards.
    assert!(result.is_ok());
}
