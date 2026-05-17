use super::*;
use crate::harness::PendingTool;

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
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::AgentStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("unavailable tool should be rejected cleanly");

    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().any(|node| {
        matches!(
            &node.entry,
            SessionEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message == "tool is not available"
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
        Event::AgentResponseFinished(AgentResponseFinished {
            session_prompt_id: "sp-main".into(),
            output_items: vec![ContextItem::ToolCall(ToolCallItem {
                call_id: call_id.clone(),
                name: tool_name.clone(),
                tool_type: tau_proto::ToolType::Function,
                arguments: CborValue::Map(Vec::new()),
            })],
            stop_reason: tau_proto::AgentStopReason::ToolCalls,
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
            tool_type: tau_proto::ToolType::Function,
        },
    );
    h.pending_tool_providers
        .insert(call_id.clone(), conn_id.clone().into());
    h.in_flight_tool_kinds
        .insert(call_id.clone(), tau_proto::ToolSideEffects::Mutating);
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
                                if message == "tool provider disconnected"
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
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );
    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::AgentStopReason::ToolCalls,
        usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("removed tool should be rejected cleanly");

    let session = h.store.session("s1").expect("session");
    assert!(session.nodes().iter().any(|node| {
        matches!(
            &node.entry,
            SessionEntry::ToolResults { items }
                if items.iter().any(|item| {
                    item.call_id.as_str() == "c1"
                        && matches!(
                            &item.status,
                            ToolResultStatus::Error { message } if message == "tool is not available"
                        )
                })
        )
    }));
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
        config_dir.join("harness.json5"),
        r#"{
            toolsProfiles: {
                read_only: {
                    shell: false,
                },
            },
            roles: {
                smart: { toolsProfile: "read_only" },
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
    h.selected_role = "smart".to_owned();
    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.publish_for_conversation(
        &cid,
        Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "do it".to_owned(),
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    h.handle_agent_response_finished(AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: tau_proto::ToolName::new("shell"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::AgentStopReason::ToolCalls,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::AgentTokenUsage {
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
    assert!(injected.contains("# AGENTS.md instructions for /repo/pkg"));
    assert!(injected.contains("# AGENTS.md instructions for /repo"));
    assert!(injected.contains("<INSTRUCTIONS>"));
    assert!(injected.contains("</INSTRUCTIONS>"));
    let root_pos = injected.find("root rule").expect("root rule");
    let pkg_pos = injected.find("package rule").expect("package rule");
    assert!(
        root_pos < pkg_pos,
        "broader file should appear before nested one"
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
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = AgentResponseFinished {
        session_prompt_id: "sp-x".into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: "c1".into(),
            name: ToolName::new("not_a_tool"),
            tool_type: tau_proto::ToolType::Function,
            arguments: CborValue::Map(Vec::new()),
        })],
        stop_reason: tau_proto::AgentStopReason::ToolCalls,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::AgentTokenUsage {
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
                                if message.contains("tool is not available")
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
            side_effects: ToolSideEffects::Mutating,
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
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        }),
    );

    let response = AgentResponseFinished {
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
        stop_reason: tau_proto::AgentStopReason::ToolCalls,
        usage: match (None, None, None) {
            (None, None, None) => None,
            (input_tokens, cached_tokens, output_tokens) => Some(tau_proto::AgentTokenUsage {
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
        .handle_agent_response_finished(response)
        .expect_err("empty call id must reject the response");
    assert!(
        error.to_string().contains("empty call_id"),
        "unexpected error: {error}"
    );

    // No tool work should be scheduled and the malformed assistant
    // response should not be committed to the session tree.
    assert!(h.pending_tool_invocations.is_empty());
    assert!(h.in_flight_tool_kinds.is_empty());
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

    h.handle_agent_response_finished(AgentResponseFinished {
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
        stop_reason: tau_proto::AgentStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");

    assert!(h.pending_tool_invocations.is_empty());
    assert!(h.in_flight_tool_kinds.is_empty());
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
    use tau_proto::ToolSideEffects::{Mutating, Pure};

    let td = TempDir::new().expect("tempdir");
    let sp = td.path().join("state");
    let mut h = echo_harness(&sp).expect("start");
    h.selected_model = Some("test/model".into());

    let cid = h.default_conversation_id.clone();
    seed_agent_thinking(&mut h, &cid, "sp-x");
    h.prompt_conversations.insert("sp-x".into(), cid.clone());
    h.handle_agent_response_finished(AgentResponseFinished {
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
        stop_reason: tau_proto::AgentStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    })
    .expect("response");
    assert_eq!(
        h.in_flight_tool_kinds.get(&ToolCallId::from("c1")),
        Some(&Pure)
    );
    assert_eq!(h.pending_tool_invocations[0].1.id, "c2");
    assert_eq!(h.pending_tool_invocations[0].2, Mutating);

    h.handle_client_event(
        "ui",
        Frame::Event(Event::UiCancelPrompt(tau_proto::UiCancelPrompt {
            session_id: "s1".into(),
            session_prompt_id: None,
        })),
    )
    .expect("cancel");

    assert!(h.pending_tool_invocations.is_empty());
    assert!(h.in_flight_tool_kinds.is_empty());
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
            display: None,
            originator: tau_proto::PromptOriginator::User,
        })),
    );
    // Should not error — just emits a warning and discards.
    assert!(result.is_ok());
}
