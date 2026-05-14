use tau_proto::{
    AgentResponseFinished, CborValue, ClientKind, ConnectionId, Event, EventName, EventSelector,
    Frame, ToolRegister, ToolRequest, ToolResult, ToolSideEffects, ToolSpec, UiNavigateTree,
    UiPromptSubmitted,
};
use tempfile::TempDir;

use crate::bus::EventBus;
use crate::connection::{
    Connection, ConnectionMetadata, ConnectionOrigin, RouteError, RoutedFrame,
};
use crate::memory::{MemoryInbox, MemorySink, memory_connection};
use crate::policy::{DefaultSubscriptionPolicy, PolicyStore, SubscriptionApproval};
use crate::session::{NodeId, SessionEntry, ToolActivityOutcome, ToolActivityRecord};
use crate::session_store::SessionStore;
use crate::tool_registry::{ToolRegistry, ToolRegistryWarning};

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
            Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: format!("sp-{session_id}-{text}").into(),
                text: Some(text.to_owned()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                output_tokens: None,
                thinking: None,
                token_usage: None,
                originator: tau_proto::PromptOriginator::User,

                backend: None,
                response_id: None,
                phase: None,
                reasoning_items: Vec::new(),
                ws_pool_delta: None,
            }),
        )
        .expect("append session event");
    store
        .session(session_id)
        .and_then(|t| t.head())
        .expect("head after append")
}

#[test]
fn subscribed_clients_only_receive_matching_events() {
    let mut bus = EventBus::new();

    let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
    let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let agent_id = bus.connect(agent_connection);
    let ui_id = bus.connect(ui_connection);

    bus.set_subscriptions(
        &agent_id,
        vec![EventSelector::Exact(EventName::UI_PROMPT_SUBMITTED)],
    )
    .expect("agent subscriptions should be stored");
    bus.set_subscriptions(&ui_id, vec![EventSelector::Prefix("tool.".to_owned())])
        .expect("ui subscriptions should be stored");

    let report = bus.publish(Frame::Event(Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "s1".into(),
        text: "hello".to_owned(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    })));

    assert_eq!(report.delivered_to, vec![agent_id.clone()]);
    assert_eq!(report.skipped_by_subscription, vec![ui_id.clone()]);
    assert_eq!(agent_inbox.snapshot().len(), 1);
    assert!(ui_inbox.snapshot().is_empty());
}

#[test]
fn directed_events_ignore_subscriptions_but_still_use_visibility_filters() {
    let mut bus = EventBus::new();

    let (ui_connection, ui_inbox) = memory_connection("ui", ClientKind::Ui);
    let filtered_connection =
        ui_connection.with_visibility_filter(Box::new(|routed: &RoutedFrame| {
            matches!(
                &routed.frame,
                Frame::Event(event) if event.name() == EventName::TOOL_INVOKE
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
            Frame::Event(Event::AgentResponseFinished(AgentResponseFinished {
                session_prompt_id: "sp-1".into(),
                text: Some("hidden".to_owned()),
                tool_calls: Vec::new(),
                input_tokens: None,
                cached_tokens: None,
                output_tokens: None,
                thinking: None,
                token_usage: None,
                originator: tau_proto::PromptOriginator::User,

                backend: None,
                response_id: None,
                phase: None,
                reasoning_items: Vec::new(),
                ws_pool_delta: None,
            })),
        )
        .expect("directed route should succeed");
    assert_eq!(blocked.blocked_by_filter, vec![ui_id.clone()]);
    assert!(ui_inbox.snapshot().is_empty());

    let delivered = bus
        .send_to(
            &ui_id,
            Some(&tool_id),
            Frame::Event(Event::ToolInvoke(tau_proto::ToolInvoke {
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

    let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let agent_id = bus.connect(agent_connection);
    let tool_id = bus.connect(tool_connection);

    bus.set_subscriptions(&agent_id, vec![EventSelector::Prefix("agent.".to_owned())])
        .expect("agent subscriptions should be stored");
    bus.set_subscriptions(&tool_id, vec![EventSelector::Prefix("tool.".to_owned())])
        .expect("tool subscriptions should be stored");

    let first_report = bus.publish(Frame::Event(Event::ToolResult(tau_proto::ToolResult {
        call_id: "call-1".into(),
        tool_name: tau_proto::ToolName::new("echo"),
        result: CborValue::Text("done".to_owned()),
        display: None,
        originator: tau_proto::PromptOriginator::User,
    })));
    assert_eq!(first_report.delivered_to, vec![tool_id.clone()]);

    let second_report = bus.publish(Frame::Event(Event::AgentResponseFinished(
        AgentResponseFinished {
            session_prompt_id: "sp-1".into(),
            text: Some("done".to_owned()),
            tool_calls: Vec::new(),
            input_tokens: None,
            cached_tokens: None,
            output_tokens: None,
            thinking: None,
            token_usage: None,
            originator: tau_proto::PromptOriginator::User,

            backend: None,
            response_id: None,
            phase: None,
            reasoning_items: Vec::new(),
            ws_pool_delta: None,
        },
    )));
    assert_eq!(second_report.delivered_to, vec![agent_id.clone()]);

    assert_eq!(tool_inbox.snapshot().len(), 1);
    assert_eq!(agent_inbox.snapshot().len(), 1);
}

#[test]
fn provider_can_register_tool_and_receive_invocations() {
    let mut bus = EventBus::new();
    let mut registry = ToolRegistry::new();

    let (agent_connection, agent_inbox) = memory_connection("agent", ClientKind::Agent);
    let (tool_connection, tool_inbox) = memory_connection("tool", ClientKind::Tool);
    let agent_id = bus.connect(agent_connection);
    let tool_id = bus.connect(tool_connection);

    let register_report = registry.register(
        &tool_id,
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            description: Some("Echo a payload".to_owned()),
            parameters: None,
            enabled_by_default: true,
            side_effects: ToolSideEffects::Pure,
        },
    );
    assert!(register_report.warnings.is_empty());

    let route_report = registry
        .route_tool_request(
            &mut bus,
            &agent_id,
            ToolRequest {
                call_id: "call-1".into(),
                tool_name: tau_proto::ToolName::new("echo"),
                arguments: CborValue::Text("hello".to_owned()),
                originator: tau_proto::PromptOriginator::User,
            },
        )
        .expect("tool request should route");

    assert_eq!(route_report.provider_connection_id, tool_id.clone());
    assert_eq!(
        route_report.route_report.delivered_to,
        vec![tool_id.clone()]
    );
    assert!(agent_inbox.snapshot().is_empty());

    let delivered_events = tool_inbox.snapshot();
    assert_eq!(delivered_events.len(), 1);
    assert_eq!(delivered_events[0].source_id, Some(agent_id));
    assert_eq!(
        delivered_events[0].frame,
        Frame::Event(Event::ToolInvoke(tau_proto::ToolInvoke {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("echo"),
            arguments: CborValue::Text("hello".to_owned()),
            originator: tau_proto::PromptOriginator::User,
        }))
    );
}

#[test]
fn duplicate_tool_registrations_warn_but_remain_available() {
    let mut registry = ToolRegistry::new();

    let first_report = registry.register(
        "conn-a",
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            description: Some("Echo".to_owned()),
            parameters: None,
            enabled_by_default: true,
            side_effects: ToolSideEffects::Pure,
        },
    );
    assert!(first_report.warnings.is_empty());

    let second_report = registry.register(
        "conn-b",
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            description: Some("Echo from another provider".to_owned()),
            parameters: None,
            enabled_by_default: true,
            side_effects: ToolSideEffects::Pure,
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
            description: None,
            parameters: None,
            enabled_by_default: true,
            side_effects: ToolSideEffects::Pure,
        },
    );
    registry.register(
        &second_id,
        ToolSpec {
            name: tau_proto::ToolName::new("echo"),
            description: None,
            parameters: None,
            enabled_by_default: true,
            side_effects: ToolSideEffects::Pure,
        },
    );
    registry.register(
        &first_id,
        ToolSpec {
            name: tau_proto::ToolName::new("demo_upper"),
            description: None,
            parameters: None,
            enabled_by_default: true,
            side_effects: ToolSideEffects::Pure,
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
fn register_events_map_cleanly_to_registry_state() {
    let mut registry = ToolRegistry::new();

    let report = registry.register(
        "conn-tool",
        ToolRegister {
            tool: ToolSpec {
                name: tau_proto::ToolName::new("echo"),
                description: Some("Echo".to_owned()),
                parameters: None,
                enabled_by_default: true,
                side_effects: ToolSideEffects::Pure,
            },
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
                originator: tau_proto::PromptOriginator::User,
                ctx_id: None,
            }),
        )
        .expect("append after reopen");
    assert_eq!(outcome.id.get(), 16);
}

/// `AgentResponseFinished.phase` must survive the session-event
/// fold into `SessionEntry::AgentMessage`. The next prompt-assembly
/// pass needs the value to echo it back on the wire — without this,
/// every turn after a compaction (or a stale-chain fallback) would
/// drop the label and re-trigger the early-stopping bug the OpenAI
/// deployment checklist warns about.
#[test]
fn session_tree_captures_phase_from_agent_response_finished() {
    use tau_proto::MessagePhase;

    let mut tree = crate::session::SessionTree::from_events("session-1".into(), &[]);
    tree.apply_event(&Event::UiPromptSubmitted(UiPromptSubmitted {
        session_id: "session-1".into(),
        text: "hello".to_owned(),
        originator: tau_proto::PromptOriginator::User,
        ctx_id: None,
    }));
    tree.apply_event(&Event::AgentResponseFinished(AgentResponseFinished {
        session_prompt_id: "sp-1".into(),
        text: Some("draft response".to_owned()),
        tool_calls: Vec::new(),
        input_tokens: None,
        cached_tokens: None,
        output_tokens: None,
        thinking: None,
        token_usage: None,
        originator: tau_proto::PromptOriginator::User,
        backend: None,
        response_id: None,
        phase: Some(MessagePhase::Commentary),
        reasoning_items: Vec::new(),
        ws_pool_delta: None,
    }));

    let last = tree
        .current_branch()
        .into_iter()
        .last()
        .expect("at least one entry");
    match last {
        SessionEntry::AgentMessage { phase, text, .. } => {
            assert_eq!(text.as_deref(), Some("draft response"));
            assert_eq!(*phase, Some(MessagePhase::Commentary));
        }
        other => panic!("expected AgentMessage, got {other:?}"),
    }
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
            &SessionEntry::UserMessage {
                text: "hello".to_owned(),
            },
            &SessionEntry::AgentMessage {
                text: Some("hi there".to_owned()),
                thinking: None,
                phase: None,
                reasoning_items: Vec::new(),
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
            &SessionEntry::UserMessage {
                text: "hello".to_owned(),
            },
            &SessionEntry::UserMessage {
                text: "goodbye".to_owned(),
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
fn session_tree_associates_tool_activity() {
    let tempdir = TempDir::new().expect("tempdir should exist");
    let store_path = tempdir.path().join("state");

    let mut store = SessionStore::open(&store_path).expect("store should open");
    let _ = store_user_message(&mut store, "session-1", "read a file");
    store
        .append_session_event(
            "session-1",
            None,
            Event::ToolResult(ToolResult {
                call_id: "call-1".into(),
                tool_name: tau_proto::ToolName::new("read"),
                result: CborValue::Text("README".to_owned()),
                display: None,
                originator: tau_proto::PromptOriginator::User,
            }),
        )
        .expect("tool result event should persist");

    let reopened = SessionStore::open(&store_path).expect("store should reopen");
    let tree = reopened
        .session("session-1")
        .expect("session should reload");
    let branch = tree.current_branch();
    assert_eq!(branch.len(), 2);
    assert_eq!(
        *branch[1],
        SessionEntry::ToolActivity(ToolActivityRecord {
            call_id: "call-1".into(),
            tool_name: tau_proto::ToolName::new("read"),
            outcome: ToolActivityOutcome::Result {
                result: CborValue::Text("README".to_owned()),
            },
        })
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
