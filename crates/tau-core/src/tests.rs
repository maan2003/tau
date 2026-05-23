use tau_proto::{
    CborValue, ClientKind, ConnectionId, ContentPart, ContextItem, ContextRole, Event, EventName,
    EventSelector, Frame, MessageItem, PromptOriginator, ProviderResponseFinished,
    SessionPromptSteered, SessionUserMessageInjected, ToolCallItem, ToolExecutionMode,
    ToolRegister, ToolRequest, ToolResult, ToolResultStatus, ToolSpec, ToolType, UiNavigateTree,
    UiPromptSubmitted,
};
use tempfile::TempDir;

use crate::bus::EventBus;
use crate::connection::{
    Connection, ConnectionMetadata, ConnectionOrigin, RouteError, RoutedFrame,
};
use crate::memory::{MemoryInbox, MemorySink, memory_connection};
use crate::policy::{DefaultSubscriptionPolicy, PolicyStore, SubscriptionApproval};
use crate::session::{NodeId, SessionEntry};
use crate::session_store::{SessionStore, SessionStoreError, list_session_metas};
use crate::tool_registry::{ToolRegistry, ToolRegistryWarning, validate_tool_arguments};

/// Helper used by the SessionStore-focused unit tests below: append
/// one `UiPromptSubmitted` event and return the resulting head.
/// Production code never calls anything like this — it goes through
/// the harness, which publishes events. Tests get the same effect by
/// driving `append_session_event` directly.
fn store_user_message(store: &mut SessionStore, session_id: &str, text: &str) -> NodeId {
    store
        .append_session_event(
            session_id,
            None,
            Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: session_id.into(),
                text: text.to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
        )
        .expect("append session event");
    store
        .session(session_id)
        .and_then(|t| t.head())
        .expect("head after append")
}

fn store_agent_message(store: &mut SessionStore, session_id: &str, text: &str) -> NodeId {
    store
        .append_session_event(
            session_id,
            None,
            Event::ProviderResponseFinished(provider_response_text(
                &format!("sp-{session_id}-{text}"),
                text,
            )),
        )
        .expect("append session event");
    store
        .session(session_id)
        .and_then(|t| t.head())
        .expect("head after append")
}

fn assistant_message_item(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::Assistant,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
    })
}

fn user_message_item(text: &str) -> ContextItem {
    ContextItem::Message(MessageItem {
        role: ContextRole::User,
        content: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        phase: None,
    })
}

fn provider_response_text(session_prompt_id: &str, text: &str) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        output_items: vec![assistant_message_item(text)],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn provider_response_tool_call(session_prompt_id: &str, call_id: &str) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        output_items: vec![ContextItem::ToolCall(ToolCallItem {
            call_id: call_id.into(),
            name: tau_proto::ToolName::new("read"),
            tool_type: ToolType::Function,
            arguments: CborValue::Null,
        })],
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

fn provider_response_tool_calls(
    session_prompt_id: &str,
    call_ids: &[&str],
) -> ProviderResponseFinished {
    ProviderResponseFinished {
        session_prompt_id: session_prompt_id.into(),
        output_items: call_ids
            .iter()
            .map(|call_id| {
                ContextItem::ToolCall(ToolCallItem {
                    call_id: (*call_id).into(),
                    name: tau_proto::ToolName::new("read"),
                    tool_type: ToolType::Function,
                    arguments: CborValue::Null,
                })
            })
            .collect(),
        stop_reason: tau_proto::ProviderStopReason::ToolCalls,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }
}

#[test]
fn subscribed_clients_only_receive_matching_events() {
    let mut bus = EventBus::new();

    let (provider_connection, provider_inbox) = memory_connection("provider", ClientKind::Provider);
    let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let provider_id = bus.connect(provider_connection);
    let ui_id = bus.connect(ui_connection);

    bus.set_subscriptions(
        &provider_id,
        vec![EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)],
    )
    .expect("provider subscriptions should be stored");
    bus.set_subscriptions(&ui_id, vec![EventSelector::Prefix("tool.".to_owned())])
        .expect("ui subscriptions should be stored");

    let report = bus.publish(Frame::Event(Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })));

    assert_eq!(report.delivered_to, vec![provider_id.clone()]);
    assert_eq!(report.skipped_by_subscription, vec![ui_id.clone()]);
    assert_eq!(provider_inbox.snapshot().len(), 1);
    assert!(ui_inbox.snapshot().is_empty());
}

#[test]
fn broadcast_can_skip_execution_client_kinds_for_direct_prompt_routing() {
    // Provider-owned prompts are logged for observers but sent directly to the
    // owning provider. The bus helper must keep UI/tool subscribers informed
    // without waking every execution client that happens to subscribe.
    let mut bus = EventBus::new();

    let (observer_connection, observer_inbox) =
        memory_connection("provider-observer", ClientKind::Provider);
    let (provider_connection, provider_inbox) =
        memory_connection("provider-owner", ClientKind::Provider);
    let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let observer_id = bus.connect(observer_connection);
    let provider_id = bus.connect(provider_connection);
    let ui_id = bus.connect(ui_connection);

    for id in [&observer_id, &provider_id, &ui_id] {
        bus.set_subscriptions(
            id,
            vec![EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)],
        )
        .expect("subscriptions should be stored");
    }

    let report = bus.publish_from_excluding_kinds(
        None,
        Frame::Event(Event::UiPromptSubmitted(UiPromptSubmitted {
            session_id: "s1".into(),
            text: "hello".to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        })),
        &[ClientKind::Provider],
    );

    assert_eq!(report.delivered_to, vec![ui_id.clone()]);
    assert!(report.skipped_by_subscription.contains(&observer_id));
    assert!(report.skipped_by_subscription.contains(&provider_id));
    assert!(observer_inbox.snapshot().is_empty());
    assert!(provider_inbox.snapshot().is_empty());
    assert_eq!(ui_inbox.snapshot().len(), 1);
}

#[test]
fn directed_events_ignore_subscriptions_but_still_use_visibility_filters() {
    let mut bus = EventBus::new();

    let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let filtered_connection =
        ui_connection.with_visibility_filter(Box::new(|routed: &RoutedFrame| {
            matches!(
                &routed.frame,
                Frame::Event(event) if event.name() == EventName::TOOL_STARTED
            )
        }));
    let ui_id = bus.connect(filtered_connection);

    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let tool_id = bus.connect(tool_connection);
    bus.set_subscriptions(&tool_id, Vec::new())
        .expect("tool subscriptions should be stored");

    let blocked = bus
        .send_to(
            &ui_id,
            Some(&tool_id),
            Frame::Event(Event::ProviderResponseFinished(provider_response_text(
                "sp-1", "hidden",
            ))),
        )
        .expect("directed route should succeed");
    assert_eq!(blocked.blocked_by_filter, vec![ui_id.clone()]);
    assert!(ui_inbox.snapshot().is_empty());

    let delivered = bus
        .send_to(
            &ui_id,
            Some(&tool_id),
            Frame::Event(Event::ToolStarted(tau_proto::ToolStarted {
                call_id: "call-1".into(),
                tool_name: tau_proto::ToolName::new("echo"),
                arguments: CborValue::Null,
                originator: tau_proto::PromptOriginator::User,
            })),
        )
        .expect("directed route should succeed");
    assert_eq!(delivered.delivered_to, vec![ui_id.clone()]);
    assert_eq!(ui_inbox.snapshot().len(), 1);
    assert!(tool_inbox.snapshot().is_empty());
}

#[test]
fn connection_abstraction_is_transport_independent_for_in_memory_clients() {
    let mut bus = EventBus::new();

    let (provider_connection, provider_inbox) = memory_connection("provider", ClientKind::Provider);
    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let provider_id = bus.connect(provider_connection);
    let tool_id = bus.connect(tool_connection);

    bus.set_subscriptions(
        &provider_id,
        vec![EventSelector::Prefix("provider.".to_owned())],
    )
    .expect("provider subscriptions should be stored");
    bus.set_subscriptions(&tool_id, vec![EventSelector::Prefix("tool.".to_owned())])
        .expect("tool subscriptions should be stored");

    let first_report = bus.publish(Frame::Event(Event::ToolResult(tau_proto::ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("echo"),
        tool_type: ToolType::Function,
        result: CborValue::Text("done".to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    })));
    assert_eq!(first_report.delivered_to, vec![tool_id.clone()]);

    let second_report = bus.publish(Frame::Event(Event::ProviderResponseFinished(
        provider_response_text("sp-1", "done"),
    )));
    assert_eq!(second_report.delivered_to, vec![provider_id.clone()]);

    assert_eq!(tool_inbox.snapshot().len(), 1);
    assert_eq!(provider_inbox.snapshot().len(), 1);
}

fn schema_test_tool(parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: tau_proto::ToolName::new("schema_test"),
        model_visible_name: None,
        description: None,
        tool_type: ToolType::Function,
        parameters: Some(parameters),
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    }
}

#[test]
fn tool_argument_validation_rejects_unknown_top_level_properties() {
    let tool = schema_test_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" }
        },
        "required": ["path"],
        "additionalProperties": false
    }));
    let err = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![
            (
                CborValue::Text("path".to_owned()),
                CborValue::Text("/tmp/file".to_owned()),
            ),
            (CborValue::Text("extra".to_owned()), CborValue::Bool(true)),
        ]),
    )
    .expect_err("extra argument should fail");

    assert_eq!(err.to_string(), "unexpected argument `extra`");
}

#[test]
fn tool_argument_validation_checks_nested_items() {
    let tool = schema_test_tool(serde_json::json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": { "type": "string" },
                        "max_matches": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["oldText"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["edits"],
        "additionalProperties": false
    }));
    let err = validate_tool_arguments(
        &tool,
        &CborValue::Map(vec![(
            CborValue::Text("edits".to_owned()),
            CborValue::Array(vec![CborValue::Map(vec![
                (
                    CborValue::Text("oldText".to_owned()),
                    CborValue::Text("before".to_owned()),
                ),
                (
                    CborValue::Text("max_matches".to_owned()),
                    CborValue::Integer(0.into()),
                ),
            ])]),
        )]),
    )
    .expect_err("nested minimum should fail");

    assert_eq!(
        err.to_string(),
        "$.edits[0].max_matches: must be at least 1"
    );
}

#[test]
fn provider_can_register_tool_and_receive_invocations() {
    let mut bus = EventBus::new();
    let mut registry = ToolRegistry::new();

    let (provider_connection, provider_inbox) = memory_connection("provider", ClientKind::Provider);
    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let _provider_id = bus.connect(provider_connection);
    let tool_id = bus.connect(tool_connection);

    let register_report = registry.register(
        &tool_id,
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            model_visible_name: None,
            description: Some("Echo a payload".to_owned()),
            tool_type: ToolType::Function,
            parameters: None,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    assert!(register_report.warnings.is_empty());

    let route_report = registry
        .route_tool_request(ToolRequest {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("echo"),
            tool_type: ToolType::Function,
            arguments: CborValue::Text("hello".to_owned()),
            originator: tau_proto::PromptOriginator::User,
        })
        .expect("tool tool request should route");

    assert_eq!(route_report.provider_connection_id, tool_id.clone());
    assert!(provider_inbox.snapshot().is_empty());
    assert!(tool_inbox.snapshot().is_empty());
    assert_eq!(
        route_report.invoke,
        tau_proto::ToolStarted {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("echo"),
            arguments: CborValue::Text("hello".to_owned()),
            originator: tau_proto::PromptOriginator::User,
        }
    );
}

#[test]
fn duplicate_tool_registrations_warn_but_remain_available() {
    let mut registry = ToolRegistry::new();

    let first_report = registry.register(
        "conn-a",
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            model_visible_name: None,
            description: Some("Echo".to_owned()),
            tool_type: ToolType::Function,
            parameters: None,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    assert!(first_report.warnings.is_empty());

    let second_report = registry.register(
        "conn-b",
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            model_visible_name: None,
            description: Some("Echo from another provider".to_owned()),
            tool_type: ToolType::Function,
            parameters: None,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    assert_eq!(second_report.warnings.len(), 1);
    assert_eq!(
        second_report.warnings[0],
        ToolRegistryWarning::DuplicateRegistration {
            tool_name: tau_proto::ToolName::new("echo"),
            existing_provider_ids: vec!["conn-a".into()],
        }
    );

    let providers = registry.providers_for("echo");
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0].connection_id, "conn-a");
    assert_eq!(providers[1].connection_id, "conn-b");
}

#[test]
fn disconnect_cleanup_removes_stale_tool_providers() {
    let mut bus = EventBus::new();
    let mut registry = ToolRegistry::new();

    let (first_connection, _first_inbox) = memory_connection("tool-a", ClientKind::Tool);
    let (second_connection, _second_inbox) = memory_connection("tool-b", ClientKind::Tool);
    let first_id = bus.connect(first_connection);
    let second_id = bus.connect(second_connection);

    registry.register(
        &first_id,
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            model_visible_name: None,
            description: None,
            tool_type: ToolType::Function,
            parameters: None,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    registry.register(
        &second_id,
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            model_visible_name: None,
            description: None,
            tool_type: ToolType::Function,
            parameters: None,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );
    registry.register(
        &first_id,
        ToolSpec {
            name: tau_proto::ToolName::new("demo_upper"),
            model_visible_name: None,
            description: None,
            tool_type: ToolType::Function,
            parameters: None,
            format: None,
            enabled_by_default: true,
            execution_mode: ToolExecutionMode::Shared,
            background_support: None,
        },
    );

    let disconnected = bus.disconnect(&first_id);
    assert!(disconnected.is_some());
    let removed_tools = registry.unregister_connection(&first_id);
    assert_eq!(removed_tools.len(), 2);
    assert!(removed_tools.iter().any(|tool_name| tool_name == "echo"));
    assert!(
        removed_tools
            .iter()
            .any(|tool_name| tool_name == "demo_upper")
    );

    let echo_providers = registry.providers_for("echo");
    assert_eq!(echo_providers.len(), 1);
    assert_eq!(echo_providers[0].connection_id, second_id);
    assert!(registry.providers_for("demo_upper").is_empty());
}

#[test]
fn re_registering_tool_without_fragment_clears_previous_prompt_fragment() {
    // Prompt fragments are part of the live registration, not sticky metadata. A
    // tool that unregisters or re-registers without a fragment must not leave stale
    // prompt text behind for future prompt assembly.
    let mut registry = ToolRegistry::new();
    let tool = || ToolSpec {
        name: tau_proto::ToolName::new("echo"),
        model_visible_name: None,
        description: Some("Echo".to_owned()),
        tool_type: ToolType::Function,
        parameters: None,
        format: None,
        enabled_by_default: true,
        execution_mode: ToolExecutionMode::Shared,
        background_support: None,
    };

    registry.register_with_prompt_fragment(
        "conn-tool",
        ToolRegister {
            tool: tool(),
            prompt_fragment: Some(tau_proto::PromptFragment::new(
                "echo.instructions",
                tau_proto::PromptPriority::new(10),
                "STALE TOOL PROMPT",
            )),
        },
    );
    assert!(registry.providers_for("echo")[0].prompt_fragment.is_some());

    registry.register_with_prompt_fragment(
        "conn-tool",
        ToolRegister {
            tool: tool(),
            prompt_fragment: None,
        },
    );
    assert_eq!(registry.providers_for("echo")[0].prompt_fragment, None);

    assert!(registry.unregister("conn-tool", "echo"));
    registry.register_with_prompt_fragment(
        "conn-tool",
        ToolRegister {
            tool: tool(),
            prompt_fragment: None,
        },
    );
    assert_eq!(registry.providers_for("echo")[0].prompt_fragment, None);
}

#[test]
fn register_events_map_cleanly_to_registry_state() {
    let mut registry = ToolRegistry::new();

    let report = registry.register(
        "conn-tool",
        ToolRegister {
            tool: ToolSpec {
                name: tau_proto::ToolName::new("echo"),
                model_visible_name: None,
                description: Some("Echo".to_owned()),
                tool_type: ToolType::Function,
                parameters: None,
                format: None,
                enabled_by_default: true,
                execution_mode: ToolExecutionMode::Shared,
                background_support: None,
            },
            prompt_fragment: None,
        }
        .tool,
    );

    assert!(report.warnings.is_empty());
    assert_eq!(registry.providers_for("echo").len(), 1);
    assert!(registry.unregister("conn-tool", "echo"));
    assert!(registry.providers_for("echo").is_empty());
}

#[test]
fn explicit_parent_preserved_across_replay() {
    // Phase 4 of the interception refactor: when a publish goes
    // through `append_session_event_at`, the explicit parent must
    // round-trip through the durable log so a fresh replay rebuilds
    // exactly the same branching topology — even when sibling
    // nodes were appended without intervening `UiNavigateTree`
    // events to bounce the write cursor.
    let tempdir = TempDir::new().expect("tempdir");
    let store_path = tempdir.path().join("state");

    let session_id = "branching-session";
    let user_event = |text: &str| {
        Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
            session_id: session_id.into(),
            text: text.to_owned(),
            message_class: tau_proto::PromptMessageClass::User,
            originator: tau_proto::PromptOriginator::User,
            ctx_id: None,
        })
    };

    {
        let mut store = SessionStore::open(&store_path).expect("open");
        // Three siblings: each one parents off the root (NodeId::new(0))
        // by passing the parent explicitly. There are no
        // `UiNavigateTree` events in between.
        store
            .append_session_event_at(
                session_id,
                None,
                None,
                user_event("root"),
                tau_proto::UnixMicros::now(),
            )
            .expect("root");
        store
            .append_session_event_at(
                session_id,
                None,
                Some(Some(NodeId::new(0))),
                user_event("branch-a"),
                tau_proto::UnixMicros::now(),
            )
            .expect("branch-a");
        store
            .append_session_event_at(
                session_id,
                None,
                Some(Some(NodeId::new(0))),
                user_event("branch-b"),
                tau_proto::UnixMicros::now(),
            )
            .expect("branch-b");
    }

    let reopened = SessionStore::open(&store_path).expect("reopen");
    let tree = reopened.session(session_id).expect("session reload");

    // Three nodes, all parented off NodeId::new(0).
    assert_eq!(tree.nodes().len(), 3);
    assert_eq!(
        tree.node(NodeId::new(0)).expect("root node").parent_id,
        None,
    );
    assert_eq!(
        tree.node(NodeId::new(1)).expect("branch-a node").parent_id,
        Some(NodeId::new(0)),
    );
    assert_eq!(
        tree.node(NodeId::new(2)).expect("branch-b node").parent_id,
        Some(NodeId::new(0)),
    );

    // The branching is preserved: NodeId::new(0) has two children.
    let mut children: Vec<_> = tree.children(NodeId::new(0));
    children.sort_by_key(|n| n.get());
    assert_eq!(children, vec![NodeId::new(1), NodeId::new(2)]);
}

#[test]
fn next_event_id_is_cached_across_appends_and_reopen() {
    // Regression for the O(N)-per-append re-decode bug: the next
    // event id should be tracked incrementally on the in-memory
    // `SessionTree` rather than recomputed by re-reading the on-disk
    // log, and it should be re-initialised correctly on reopen from
    // the highest persisted id.
    let tempdir = TempDir::new().expect("tempdir");
    let store_path = tempdir.path().join("state");
    let session_id = "incrementing-session";

    {
        let mut store = SessionStore::open(&store_path).expect("open");
        for i in 0..16 {
            let outcome = store
                .append_session_event(
                    session_id,
                    None,
                    Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                        session_id: session_id.into(),
                        text: format!("msg-{i}"),
                        message_class: tau_proto::PromptMessageClass::User,
                        originator: tau_proto::PromptOriginator::User,
                        ctx_id: None,
                    }),
                )
                .expect("append");
            assert_eq!(outcome.id.get(), i);
        }
        let tree = store.session(session_id).expect("loaded");
        assert_eq!(tree.next_event_id().get(), 16);
    }

    let mut reopened = SessionStore::open(&store_path).expect("reopen");
    let tree = reopened.session(session_id).expect("loaded");
    assert_eq!(tree.next_event_id().get(), 16);

    // The next append on the reopened store should pick up where we
    // left off — no gap, no collision.
    let outcome = reopened
        .append_session_event(
            session_id,
            None,
            Event::UiPromptSubmitted(tau_proto::UiPromptSubmitted {
                session_id: session_id.into(),
                text: "after-reopen".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
        )
        .expect("append after reopen");
    assert_eq!(outcome.id.get(), 16);
}

/// `ProviderResponseFinished.output_items` must survive the session-event
/// fold intact so prompt assembly can replay the exact assistant item
/// order and message metadata on later turns.
#[test]
fn session_tree_captures_phase_from_provider_response_finished() {
    use tau_proto::MessagePhase;

    let mut tree = crate::session::SessionTree::from_events("session-1".into(), &[]);
    tree.apply_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "session-1".into(),
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    tree.apply_event(&Event::ProviderResponseFinished(ProviderResponseFinished {
        session_prompt_id: "sp-1".into(),
        output_items: vec![ContextItem::Message(MessageItem {
            role: ContextRole::Assistant,
            content: vec![ContentPart::Text {
                text: "draft response".to_owned(),
            }],
            phase: Some(MessagePhase::Commentary),
        })],
        stop_reason: tau_proto::ProviderStopReason::EndTurn,
        originator: tau_proto::PromptOriginator::User,
        usage: None,
        backend: None,
        provider_response_id: None,
        ws_pool_delta: None,
    }));

    let last = tree
        .current_branch()
        .into_iter()
        .last()
        .expect("at least one entry");
    match last {
        SessionEntry::AssistantResponse { output_items, .. } => {
            assert_eq!(output_items.len(), 1);
            let ContextItem::Message(message) = &output_items[0] else {
                panic!("expected assistant message item");
            };
            assert_eq!(message.role, ContextRole::Assistant);
            assert_eq!(
                message.content,
                vec![ContentPart::Text {
                    text: "draft response".to_owned(),
                }]
            );
            assert_eq!(message.phase, Some(MessagePhase::Commentary));
        }
        other => panic!("expected AssistantResponse, got {other:?}"),
    }
}

#[test]
fn session_tree_captures_compacted_summary() {
    let mut tree = crate::session::SessionTree::from_events("session-1".into(), &[]);
    tree.apply_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "session-1".into(),
        text: "hello".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    tree.apply_event(&Event::SessionCompacted(tau_proto::SessionCompacted {
        session_id: "session-1".into(),
        originator: tau_proto::PromptOriginator::User,
        original_input_tokens: None,
        compacted_input_tokens: None,
        replacement_window: vec![assistant_message_item("summary text")],
    }));

    assert!(matches!(
        tree.current_branch().last(),
        Some(SessionEntry::Compaction { replacement_window })
            if replacement_window == &vec![assistant_message_item("summary text")]
    ));
}

#[test]
fn session_tree_persists_across_reopen() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let id0 = store_user_message(&mut store, "session-1", "hello");
    let id1 = store_agent_message(&mut store, "session-1", "hi there");

    assert_eq!(id0, NodeId::new(0));
    assert_eq!(id1, NodeId::new(1));

    let reopened = SessionStore::open(&store_path).expect("store should reopen");
    let tree = reopened
        .session("session-1")
        .expect("session should reload");
    assert_eq!(tree.head(), Some(NodeId::new(1)));
    assert_eq!(
        tree.current_branch(),
        vec![
            &SessionEntry::UserInput {
                items: vec![user_message_item("hello")],
            },
            &SessionEntry::AssistantResponse {
                provider_response_id: None,
                backend: None,
                output_items: vec![assistant_message_item("hi there")],
                usage: None,
            },
        ]
    );
    // Verify tree structure.
    assert!(
        tree.node(NodeId::new(0))
            .expect("node 0")
            .parent_id
            .is_none()
    );
    assert_eq!(
        tree.node(NodeId::new(1)).expect("node 1").parent_id,
        Some(NodeId::new(0))
    );
}

#[test]
fn session_meta_preview_tracks_only_user_authored_prompts() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");
    let session_id = "preview-session";

    let mut store = SessionStore::open(&store_path).expect("store should open");
    store_user_message(&mut store, session_id, "initial user prompt");
    store
        .append_session_event(
            session_id,
            None,
            Event::UiPromptSubmitted(UiPromptSubmitted {
                session_id: session_id.into(),
                text: "idle summary side query".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
                originator: PromptOriginator::Extension {
                    name: "std-notifications".into(),
                    query_id: "idle-0".to_owned(),
                },
                ctx_id: None,
            }),
        )
        .expect("append extension side query");
    store
        .append_session_event(
            session_id,
            None,
            Event::SessionUserMessageInjected(SessionUserMessageInjected {
                session_id: session_id.into(),
                text: "harness injected context".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
            }),
        )
        .expect("append injected message");

    let metas = list_session_metas(&store_path).expect("list session metas");
    let meta = metas
        .iter()
        .find(|(sid, _)| sid.as_str() == session_id)
        .map(|(_, meta)| meta)
        .expect("session meta exists");
    assert_eq!(
        meta.latest_user_prompt_preview.as_deref(),
        Some("initial user prompt")
    );

    store
        .append_session_event(
            session_id,
            None,
            Event::SessionPromptSteered(SessionPromptSteered {
                session_id: session_id.into(),
                text: "queued user followup".to_owned(),
                message_class: tau_proto::PromptMessageClass::User,
            }),
        )
        .expect("append queued user prompt");

    let metas = list_session_metas(&store_path).expect("list session metas");
    let meta = metas
        .iter()
        .find(|(sid, _)| sid.as_str() == session_id)
        .map(|(_, meta)| meta)
        .expect("session meta exists");
    assert_eq!(
        meta.latest_user_prompt_preview.as_deref(),
        Some("queued user followup")
    );
}

#[test]
fn session_tree_supports_branching() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store_user_message(&mut store, "s1", "hello");
    let _ = store_agent_message(&mut store, "s1", "hi");
    // Branch: navigate back to node 0, then append a different
    // message. Both happen via persisted events; the SessionTree
    // re-derives identically on reopen.
    store
        .append_session_event(
            "s1",
            None,
            Event::UiNavigateTree(UiNavigateTree {
                session_id: "s1".into(),
                node_id: 0,
            }),
        )
        .expect("navigate event");
    let _ = store_user_message(&mut store, "s1", "goodbye");

    let tree = store.session("s1").expect("session should exist");
    assert_eq!(tree.head(), Some(NodeId::new(2)));
    // Current branch: hello → goodbye (skipping "hi").
    assert_eq!(
        tree.current_branch(),
        vec![
            &SessionEntry::UserInput {
                items: vec![user_message_item("hello")],
            },
            &SessionEntry::UserInput {
                items: vec![user_message_item("goodbye")],
            },
        ]
    );
    // Node 0 has two children (branching point).
    let mut children = tree.children(NodeId::new(0));
    children.sort_by_key(|id| id.get());
    assert_eq!(children, vec![NodeId::new(1), NodeId::new(2)]);

    // Verify persistence across reopen.
    let reopened = SessionStore::open(&store_path).expect("reopen");
    let tree2 = reopened.session("s1").expect("session");
    assert_eq!(tree2.head(), Some(NodeId::new(2)));
    assert_eq!(tree2.current_branch().len(), 2);
}

#[test]
fn session_tree_folds_only_provider_tool_results_under_assistant_response() {
    // Logical `tool.result` is a UI/runtime event. Only the provider-facing
    // terminal fact closes the provider transcript round in prompt history.
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store_user_message(&mut store, "session-1", "read a file");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                session_prompt_id: "sp-tools".into(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-1".into(),
                    name: tau_proto::ToolName::new("read"),
                    tool_type: ToolType::Function,
                    arguments: CborValue::Null,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        )
        .expect("assistant response should persist");
    let result = ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: ToolType::Function,
        result: CborValue::Text("README".to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    };
    store
        .append_session_event("session-1", None, Event::ToolResult(result.clone()))
        .expect("logical tool result event should persist without folding");
    assert_eq!(
        store
            .session("session-1")
            .expect("session should load")
            .current_branch()
            .len(),
        2,
        "logical tool.result must not close the provider transcript round",
    );
    store
        .append_session_event("session-1", None, Event::ProviderToolResult(result))
        .expect("provider tool result event should persist");

    let reopened = SessionStore::open(&store_path).expect("store should reopen");
    let tree = reopened
        .session("session-1")
        .expect("session should reload");
    let branch = tree.current_branch();
    assert_eq!(branch.len(), 3);
    assert_eq!(
        *branch[2],
        SessionEntry::ToolResults {
            items: vec![tau_proto::ToolResultItem {
                call_id: "call-1".into(),
                tool_type: ToolType::Function,
                status: ToolResultStatus::Success,
                output: tau_proto::ToolResponse::from_cbor(&CborValue::Text("README".to_owned())),
            }],
        }
    );
}

/// Resume repair needs a read-only view of pending provider tool rounds. The
/// accessor must report only calls missing a terminal provider fact, including
/// partially completed parallel rounds, so callers do not duplicate completed
/// results.
#[test]
fn session_tree_reports_unresolved_foreground_tool_calls() {
    let mut tree = crate::session::SessionTree::from_events("session-1".into(), &[]);
    tree.apply_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "session-1".into(),
        text: "use tools".to_owned(),
        message_class: tau_proto::PromptMessageClass::User,
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    tree.apply_event(&Event::ProviderResponseFinished(
        provider_response_tool_calls("sp-tools", &["call-1", "call-2"]),
    ));

    let unresolved: Vec<_> = tree
        .unresolved_foreground_tool_calls_from(tree.head())
        .into_iter()
        .map(|call| call.call_id.as_str().to_owned())
        .collect();
    assert_eq!(unresolved, vec!["call-1", "call-2"]);

    tree.apply_event(&Event::ProviderToolResult(ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: ToolType::Function,
        result: CborValue::Text("first".to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    let unresolved: Vec<_> = tree
        .unresolved_foreground_tool_calls_from(tree.head())
        .into_iter()
        .map(|call| call.call_id.as_str().to_owned())
        .collect();
    assert_eq!(unresolved, vec!["call-2"]);

    tree.apply_event(&Event::ProviderToolResult(ToolResult {
        call_id: "call-2".into(),
        tool_name: tau_proto::ToolName::new("read"),
        tool_type: ToolType::Function,
        result: CborValue::Text("second".to_owned()),
        kind: tau_proto::ToolResultKind::Final,
        display: None,
        originator: tau_proto::PromptOriginator::User,
    }));
    assert!(
        tree.unresolved_foreground_tool_calls_from(tree.head())
            .is_empty()
    );
}

#[test]
fn session_tree_folds_provider_tool_result_into_prompt_history() {
    // Background placeholders are provider-facing terminal completions, not
    // logical UI tool results. They still have to close the tool round in the
    // durable prompt history so the next provider prompt is valid.
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store_user_message(&mut store, "session-1", "run slow tool");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                session_prompt_id: "sp-tools".into(),
                output_items: vec![ContextItem::ToolCall(ToolCallItem {
                    call_id: "call-1".into(),
                    name: tau_proto::ToolName::new("slow"),
                    tool_type: ToolType::Function,
                    arguments: CborValue::Null,
                })],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        )
        .expect("assistant response should persist");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderToolResult(ToolResult {
                call_id: "call-1".into(),
                tool_name: tau_proto::ToolName::new("slow"),
                tool_type: ToolType::Function,
                result: CborValue::Text("background placeholder".to_owned()),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("provider tool result event should persist");

    let tree = store.session("session-1").expect("session should load");
    let branch = tree.current_branch();
    assert_eq!(branch.len(), 3);
    assert!(matches!(branch[2], SessionEntry::ToolResults { .. }));
}

#[test]
fn session_tree_reports_unfinished_background_placeholders() {
    // Background placeholders close the provider round, while the real
    // background result/error is a separate durable event. Restore repair needs
    // to find placeholders on the current branch that never received that final
    // background event.
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store_user_message(&mut store, "session-1", "run slow tool");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(provider_response_tool_call("sp-bg", "call-bg")),
        )
        .expect("assistant response should persist");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderToolResult(ToolResult {
                call_id: "call-bg".into(),
                tool_name: tau_proto::ToolName::new("slow"),
                tool_type: ToolType::Function,
                result: CborValue::Text("background placeholder".to_owned()),
                kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("placeholder should persist");

    let events = store
        .session_events("session-1")
        .expect("session events should load");
    let tree = store.session("session-1").expect("session should load");
    let unresolved = tree.unresolved_background_tool_calls_from(tree.head(), &events);
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].call_id.as_str(), "call-bg");
    assert_eq!(unresolved[0].tool_name.as_str(), "slow");
}

#[test]
fn session_tree_ignores_finished_background_placeholders() {
    // A placeholder with a later ToolBackgroundResult or ToolBackgroundError is
    // already repaired and must not be reported as lost on resume.
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store_user_message(&mut store, "session-1", "run slow tools");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(provider_response_tool_calls(
                "sp-bg-finished",
                &["call-result", "call-error"],
            )),
        )
        .expect("assistant response should persist");
    for call_id in ["call-result", "call-error"] {
        store
            .append_session_event(
                "session-1",
                None,
                Event::ProviderToolResult(ToolResult {
                    call_id: call_id.into(),
                    tool_name: tau_proto::ToolName::new("slow"),
                    tool_type: ToolType::Function,
                    result: CborValue::Text("background placeholder".to_owned()),
                    kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            )
            .expect("placeholder should persist");
    }
    store
        .append_session_event(
            "session-1",
            None,
            Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                call_id: "call-result".into(),
                tool_name: tau_proto::ToolName::new("slow"),
                tool_type: ToolType::Function,
                result: CborValue::Text("done".to_owned()),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("background result should persist");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ToolBackgroundError(tau_proto::ToolBackgroundError {
                call_id: "call-error".into(),
                tool_name: tau_proto::ToolName::new("slow"),
                tool_type: ToolType::Function,
                message: "failed".to_owned(),
                details: None,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("background error should persist");

    let events = store
        .session_events("session-1")
        .expect("session events should load");
    let tree = store.session("session-1").expect("session should load");
    assert!(
        tree.unresolved_background_tool_calls_from(tree.head(), &events)
            .is_empty()
    );
    let states = tree.background_tool_calls_from(tree.head(), &events);
    assert_eq!(states.len(), 2);
    assert!(matches!(
        states[0].completion,
        Some(crate::BackgroundToolCompletion::Result(_))
    ));
    assert!(matches!(
        states[1].completion,
        Some(crate::BackgroundToolCompletion::Error(_))
    ));
}

/// Restored wait replay depends on completion-event order, not on the order in
/// which background placeholders closed their provider-visible tool rounds.
#[test]
fn session_tree_orders_finished_background_states_by_completion_event() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store_user_message(&mut store, "session-1", "run slow tools");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(provider_response_tool_calls(
                "sp-bg-order",
                &["call-a", "call-b"],
            )),
        )
        .expect("assistant response should persist");
    for call_id in ["call-a", "call-b"] {
        store
            .append_session_event(
                "session-1",
                None,
                Event::ProviderToolResult(ToolResult {
                    call_id: call_id.into(),
                    tool_name: tau_proto::ToolName::new("slow"),
                    tool_type: ToolType::Function,
                    result: CborValue::Text("background placeholder".to_owned()),
                    kind: tau_proto::ToolResultKind::BackgroundPlaceholder,
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            )
            .expect("placeholder should persist");
    }
    for (call_id, text) in [("call-b", "first done"), ("call-a", "second done")] {
        store
            .append_session_event(
                "session-1",
                None,
                Event::ToolBackgroundResult(tau_proto::ToolBackgroundResult {
                    call_id: call_id.into(),
                    tool_name: tau_proto::ToolName::new("slow"),
                    tool_type: ToolType::Function,
                    result: CborValue::Text(text.to_owned()),
                    display: None,
                    originator: tau_proto::PromptOriginator::User,
                }),
            )
            .expect("background result should persist");
    }

    let events = store
        .session_events("session-1")
        .expect("session events should load");
    let tree = store.session("session-1").expect("session should load");
    let states = tree.background_tool_calls_from(tree.head(), &events);

    assert_eq!(states.len(), 2);
    assert_eq!(
        states
            .iter()
            .map(|state| state.placeholder.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["call-b", "call-a"]
    );
}

#[test]
fn session_store_rejects_duplicate_tool_call_ids_before_persisting() {
    // Regression for the item-model migration: malformed provider output
    // must not be appended to the durable log before validation, because
    // replay would otherwise panic on the corrupted event.
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");
    let mut store = SessionStore::open(&store_path).expect("store should open");
    store_user_message(&mut store, "session-1", "use tools");

    let error = store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(ProviderResponseFinished {
                session_prompt_id: "sp-duplicate".into(),
                output_items: vec![
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: "call-1".into(),
                        name: tau_proto::ToolName::new("read"),
                        tool_type: ToolType::Function,
                        arguments: CborValue::Null,
                    }),
                    ContextItem::ToolCall(ToolCallItem {
                        call_id: "call-1".into(),
                        name: tau_proto::ToolName::new("read"),
                        tool_type: ToolType::Function,
                        arguments: CborValue::Null,
                    }),
                ],
                stop_reason: tau_proto::ProviderStopReason::ToolCalls,
                originator: tau_proto::PromptOriginator::User,
                usage: None,
                backend: None,
                provider_response_id: None,
                ws_pool_delta: None,
            }),
        )
        .expect_err("duplicate call id should be rejected");

    assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));
    assert_eq!(
        store
            .session_events("session-1")
            .expect("events should load")
            .len(),
        1,
        "invalid response must not be appended after the user prompt"
    );
    assert_eq!(
        store
            .session("session-1")
            .expect("session should exist")
            .current_branch()
            .len(),
        1,
        "invalid response must not mutate the cached tree"
    );
}

#[test]
fn session_store_rejects_tool_call_ids_reused_while_round_is_open_before_persisting() {
    // Reusing a call id while the earlier round is still unresolved is
    // ambiguous for terminal result matching, so reject it before the
    // malformed assistant response reaches the durable log.
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");
    let mut store = SessionStore::open(&store_path).expect("store should open");
    store_user_message(&mut store, "session-1", "use tools");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(provider_response_tool_call("sp-1", "call-1")),
        )
        .expect("first tool call should persist");

    let error = store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(provider_response_tool_call("sp-2", "call-1")),
        )
        .expect_err("call id reused while open should be rejected");

    assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));
    assert_eq!(
        store
            .session_events("session-1")
            .expect("events should load")
            .len(),
        2,
        "response reusing an open call id must not be appended"
    );
}

#[test]
fn session_store_rejects_duplicate_terminal_tool_result_before_persisting() {
    // A tool call may have exactly one terminal fact. Reject duplicates
    // before append so the durable log cannot contain facts the replay
    // projection would collapse or overwrite.
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");
    let mut store = SessionStore::open(&store_path).expect("store should open");
    store_user_message(&mut store, "session-1", "use tools");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderResponseFinished(provider_response_tool_calls(
                "sp-tools",
                &["call-1", "call-2"],
            )),
        )
        .expect("assistant response should persist");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderToolResult(ToolResult {
                call_id: "call-1".into(),
                tool_name: tau_proto::ToolName::new("read"),
                tool_type: ToolType::Function,
                result: CborValue::Text("first".to_owned()),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("first terminal result should persist");

    let error = store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderToolResult(ToolResult {
                call_id: "call-1".into(),
                tool_name: tau_proto::ToolName::new("read"),
                tool_type: ToolType::Function,
                result: CborValue::Text("duplicate".to_owned()),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect_err("duplicate terminal result should be rejected");

    assert!(matches!(error, SessionStoreError::InvalidEvent { .. }));
    assert_eq!(
        store
            .session_events("session-1")
            .expect("events should load")
            .len(),
        3,
        "duplicate terminal result must not be appended"
    );

    store
        .append_session_event(
            "session-1",
            None,
            Event::ProviderToolResult(ToolResult {
                call_id: "call-2".into(),
                tool_name: tau_proto::ToolName::new("read"),
                tool_type: ToolType::Function,
                result: CborValue::Text("second".to_owned()),
                kind: tau_proto::ToolResultKind::Final,
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("other call should still complete the round");

    let branch = store
        .session("session-1")
        .expect("session should exist")
        .current_branch();
    assert_eq!(
        **branch.last().expect("tool results node"),
        SessionEntry::ToolResults {
            items: vec![
                tau_proto::ToolResultItem {
                    call_id: "call-1".into(),
                    tool_type: ToolType::Function,
                    status: ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&CborValue::Text(
                        "first".to_owned()
                    )),
                },
                tau_proto::ToolResultItem {
                    call_id: "call-2".into(),
                    tool_type: ToolType::Function,
                    status: ToolResultStatus::Success,
                    output: tau_proto::ToolResponse::from_cbor(&CborValue::Text(
                        "second".to_owned()
                    )),
                },
            ],
        }
    );
}

#[test]
fn socket_clients_are_denied_forbidden_subscriptions() {
    let mut bus = EventBus::new();
    let inbox = MemoryInbox::default();
    let connection = Connection::new(
        ConnectionMetadata {
            id: ConnectionId::default(),
            name: "socket-ui".to_owned(),
            kind: ClientKind::Ui,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(MemorySink { inbox }),
    );
    let connection_id = bus.connect(connection);

    let error = bus
        .set_subscriptions(
            &connection_id,
            vec![EventSelector::Prefix("lifecycle.".to_owned())],
        )
        .expect_err("socket lifecycle subscription should be denied");
    assert_eq!(
        error,
        RouteError::SubscriptionDenied {
            connection_id,
            reason: "socket clients may only subscribe to allowed event families".to_owned(),
        }
    );
}

#[test]
fn policy_store_persists_allowed_socket_subscriptions() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let policy_path = tempdir.path().join("policy.cbor");
    let store = PolicyStore::open(&policy_path).expect("policy store should open");
    let mut bus =
        EventBus::with_subscription_policy(Box::new(DefaultSubscriptionPolicy::with_store(store)));
    let inbox = MemoryInbox::default();
    let connection = Connection::new(
        ConnectionMetadata {
            id: ConnectionId::default(),
            name: "socket-ui".to_owned(),
            kind: ClientKind::Ui,
            origin: ConnectionOrigin::Socket,
        },
        Box::new(MemorySink { inbox }),
    );
    let connection_id = bus.connect(connection);

    bus.set_subscriptions(
        &connection_id,
        vec![EventSelector::Prefix("tool.".to_owned())],
    )
    .expect("allowed socket subscription should persist");

    let reopened = PolicyStore::open(&policy_path).expect("policy store should reopen");
    assert_eq!(
        reopened.approvals(),
        [SubscriptionApproval {
            connection_name: "socket-ui".to_owned(),
            connection_origin: ConnectionOrigin::Socket,
            selectors: vec![EventSelector::Prefix("tool.".to_owned())],
        }]
        .as_slice()
    );
}
